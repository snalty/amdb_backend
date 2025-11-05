use noodles::core::Position;
use serde::{Deserialize, Serialize};

use aws_config::BehaviorVersion;
use axum::{http::StatusCode, response::Json, routing::get, Router};
use noodles::core::region::Interval;
use noodles::csi::binning_index::BinningIndex;
use std::env;
use std::io::Read;
use tower_service::Service;
use worker::*;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AlphamissenseResult {
    chrom: String,
    pos: i32,
    r#ref: String,
    alt: String,
    genome: String,
    uniprot_id: String,
    transcript_id: String,
    protein_variant: String,
    am_pathogenicity: f64,
    am_class: String,
}

#[derive(Deserialize)]
enum Genome {
    Hg19,
    Hg38,
}

#[derive(Deserialize)]
pub struct AlphamissenseQuery {
    chrom: String,
    pos: i32,
    ref_allele: String,
    alt_allele: String,
    genome: Genome,
}

async fn get_data_from_s3(
    genome: Genome,
    chrom: &str,
    pos: i32,
    r#ref: &str,
    alt: &str,
) -> anyhow::Result<Option<AlphamissenseResult>> {
    let (index_key, data_key) = match genome {
        Genome::Hg19 => (
            env::var("HG19_INDEX_KEY").expect("hg19 index env var should be set"),
            env::var("HG19_KEY").expect("hg19 env var should be set"),
        ),
        Genome::Hg38 => (
            env::var("HG38_INDEX_KEY").expect("env var should be set"),
            env::var("HG38_KEY").expect("env var should be set"),
        ),
    };

    let bucket = env::var("BUCKET").expect("bucket name env var should be set");

    let aws_config = aws_config::load_defaults(BehaviorVersion::latest()).await;
    let client = aws_sdk_s3::Client::new(&aws_config);

    let index_output = client
        .get_object()
        .bucket(&bucket)
        .key(index_key)
        .send()
        .await?;

    let body = index_output.body.collect().await?.to_vec();

    println!("{}", body.len());

    let mut reader = noodles::tabix::io::Reader::new(&*body);

    let index = reader.read_index()?;

    let interval = match usize::try_from(pos).map(Position::new) {
        Ok(Some(start)) => Ok(Interval::from(start..=start)),
        _ => Err(anyhow::Error::msg("Failed to create interval")),
    }?;

    let index_header = index.header().unwrap();

    println!("{:?}", index_header.reference_sequence_names());

    let sequence_index = index_header
        .reference_sequence_names()
        .get_index_of(chrom.as_bytes())
        .unwrap();

    let blocks_to_fetch = index.query(sequence_index, interval)?;

    // now let's get the blocks
    for chunk in blocks_to_fetch {
        let start = chunk.start();
        let end = chunk.end();

        println!(
            "Chunk: compressed {}..{}, uncompressed {}..{}",
            start.compressed(),
            end.compressed(),
            start.uncompressed(),
            end.uncompressed()
        );

        let data_output = client
            .get_object()
            .bucket(&bucket)
            .key(&data_key)
            .range(format!("bytes={}-{}", start.compressed(), end.compressed()))
            .send()
            .await?
            .body
            .collect()
            .await
            .unwrap()
            .to_vec();

        let mut bgzf_reader = noodles::bgzf::io::Reader::new(std::io::Cursor::new(data_output));

        let mut buf = Vec::new();

        bgzf_reader.read_to_end(&mut buf)?;

        let buf = &buf[start.uncompressed() as usize..];

        let mut csv_reader = csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .has_headers(false)
            .flexible(true)
            .from_reader(buf);

        for record in csv_reader.deserialize() {
            let record: AlphamissenseResult = record?;

            if record.chrom == chrom
                && record.pos == pos
                && record.r#ref == r#ref
                && record.alt == alt
            {
                return Ok(Some(record));
            }
        }
    }
    Ok(None)
}

#[axum::debug_handler]
pub async fn get_variant(
    Json(query): Json<AlphamissenseQuery>,
) -> axum::response::Result<Json<Option<AlphamissenseResult>>> {
    let result = get_data_from_s3(
        query.genome,
        &query.chrom,
        query.pos,
        &query.ref_allele,
        &query.alt_allele,
    )
    .await;

    match result {
        Ok(Some(r)) => Ok(Json(Some(r))),
        _ => Err(StatusCode::INTERNAL_SERVER_ERROR.into()),
    }
}

fn router() -> Router {
    Router::new().route("/", get(get_variant))
}

#[event(fetch)]
async fn fetch(
    req: HttpRequest,
    _env: Env,
    _ctx: Context,
) -> Result<http::Response<axum::body::Body>> {
    Ok(router().call(req).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_get() {
        dotenvy::dotenv().unwrap();
        let result = get_data_from_s3(Genome::Hg19, "chr1", 55523855, "G", "A")
            .await
            .unwrap();

        println!("{result:?}");
    }
}
