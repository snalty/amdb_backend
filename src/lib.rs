use anyhow::anyhow;
use noodles::core::Position;
use noodles::csi::binning_index::BinningIndex;
use noodles::{core::region::Interval, csi::binning_index::index};
use serde::{Deserialize, Serialize};
use std::hash::BuildHasher;
use std::io::Read;
use tracing::{debug, error};
use worker::*;

use rapidhash::fast::SeedableState;
use tracing_subscriber::fmt::format::Pretty;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::prelude::*;
use tracing_web::{performance_layer, MakeWebConsoleWriter};

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AlphamissenseResult {
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

#[derive(Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
enum Genome {
    Hg19,
    Hg38,
}

#[derive(Deserialize, Hash)]
pub struct AlphamissenseQuery {
    chrom: String,
    pos: i32,
    ref_allele: String,
    alt_allele: String,
    genome: Genome,
}

#[event(start)]
fn start() {
    // Initialise tracing
    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_timer(UtcTime::rfc_3339())
        .with_writer(MakeWebConsoleWriter::new().with_pretty_level());
    let perf_layer = performance_layer().with_details_from_fields(Pretty::default());
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(perf_layer)
        .init();
}

async fn get_index_from_r2(index_key: &str, bucket: &Bucket) -> anyhow::Result<Vec<u8>> {
    let index_output = bucket
        .get(index_key)
        .execute()
        .await?
        .ok_or(anyhow!(
            "Failed to get index bytes from KV with key {index_key}"
        ))
        .inspect_err(|e| error!("{}", e))?;

    debug!("Got index from R2");

    let body = index_output
        .body()
        .ok_or_else(|| anyhow!("Index response didn't have a body"))?;

    let index_bytes = body
        .bytes()
        .await
        .inspect_err(|e| error!("Failed to get body bytes with error: {e}"))?;

    Ok(index_bytes)
}

/// Get an index from R2 or the KV store
async fn get_index(
    indexes_store: &KvStore,
    index_key: &str,
    bucket: &Bucket,
) -> anyhow::Result<noodles::csi::binning_index::Index<Vec<noodles::bgzf::VirtualPosition>>> {
    let index_output = match indexes_store.get(index_key).bytes().await {
        Ok(Some(b)) => b,
        Err(e) => {
            error!(
                "Failed to get index bytes from KV store with error: {:?}",
                e
            );
            return Err(worker::Error::KvError(e).into());
        }
        Ok(None) => {
            debug!("Getting index from R2");
            let index_bytes = get_index_from_r2(index_key, bucket).await?;

            debug!("Storing index in KV for future use");

            let put_op = indexes_store
                .put_bytes(index_key, &index_bytes)
                .map_err(worker::Error::KvError)
                .inspect_err(|e| error!("Failed to put index bytes in KV with error: {}", e))?;

            put_op.execute().await.map_err(worker::Error::KvError)?;

            index_bytes
        }
    };

    let mut reader = noodles::tabix::io::Reader::new(&*index_output);

    debug!("Reading index with noodles");

    let index = reader.read_index()?;

    debug!("Index reading complete");

    Ok(index)
}

async fn get_data_from_s3(
    query: &AlphamissenseQuery,
    bucket: &Bucket,
    env: &Env,
) -> anyhow::Result<Option<AlphamissenseResult>> {
    debug!("Starting data retrieval from R2");

    // Get data URIs
    let (index_key, data_key) = match &query.genome {
        Genome::Hg19 => (env.var("HG19_INDEX_KEY")?, env.var("HG19_KEY")?),
        Genome::Hg38 => (env.var("HG38_INDEX_KEY")?, env.var("HG38_KEY")?),
    };

    let indexes_store = env.kv("INDEXES")?;

    let interval = match usize::try_from(query.pos).map(Position::new) {
        Ok(Some(start)) => Ok(Interval::from(start..=start)),
        _ => Err(anyhow::Error::msg("Failed to create interval")),
    }?;

    let index = get_index(&indexes_store, &index_key.to_string(), bucket).await?;

    let index_header = index
        .header()
        .ok_or(anyhow::Error::msg("Tabix index did not have a header!"))
        .inspect_err(|e| error!("{e}"))?;

    debug!("Getting index of reference sequence in tabix index header");
    let sequence_index = match index_header
        .reference_sequence_names()
        .get_index_of(query.chrom.as_bytes())
    {
        Some(i) => i,
        None => return Ok(None),
    };

    debug!("Getting blocks that contain ROI");
    let blocks_to_fetch = index.query(sequence_index, interval)?;

    debug!("Iterating over chunks returned");
    // now let's get the blocks
    for chunk in blocks_to_fetch {
        debug!("In chunk iteration");
        let start = chunk.start();
        let end = chunk.end();
        let len = end.compressed() - start.compressed();

        debug!("Getting required range of bytes from scores file using ranged request");
        let chunk_object_get_op = bucket
            .get(data_key.to_string())
            .range(Range::OffsetWithLength {
                offset: start.compressed(),
                length: len,
            });

        let chunk_object = chunk_object_get_op
            .execute()
            .await?
            .ok_or(anyhow::Error::msg(
                "Ranged request didn't get an Object from R2",
            ))?;

        let chunk_body = chunk_object
            .body()
            .ok_or(anyhow::Error::msg("R2 object didn't have a body"))
            .inspect_err(|e| error!("{e}"))?;

        let chunk_bytes = chunk_body
            .bytes()
            .await
            .inspect_err(|e| error!("Failed to get bytes from R2 object with error: {e}"))?;

        debug!("Got the required byte ranges");

        let mut bgzf_reader = noodles::bgzf::io::Reader::new(&*chunk_bytes);

        let mut buf = Vec::new();

        debug!("Reading bgzipped data");
        bgzf_reader.read_to_end(&mut buf)?;

        // Trim leading bytes to start position in bgzf block.
        let buf = &buf[start.uncompressed() as usize..];

        let mut csv_reader = csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .has_headers(false)
            .flexible(true)
            .from_reader(buf);

        debug!("Iterating over uncompressed records until we find the requested record");
        for record in csv_reader.deserialize() {
            let record: AlphamissenseResult = record?;
            debug!("Record: {:?}", record);

            if record.chrom == query.chrom
                && record.pos == query.pos
                && record.r#ref == query.ref_allele
                && record.alt == query.alt_allele
            {
                debug!("Got a hit for this request");
                return Ok(Some(record));
            }
        }
    }
    Ok(None)
}

async fn get_result_from_cache_or_s3(
    query: &AlphamissenseQuery,
    cache_store: &KvStore,
    bucket: &Bucket,
    env: &Env,
) -> anyhow::Result<Option<AlphamissenseResult>> {
    // Create hasher to hash our query to check KV for result
    let query_hash = SeedableState::fixed().hash_one(&query).to_string();

    let cache_response = cache_store
        .get(&query_hash)
        .json::<AlphamissenseResult>()
        .await
        .map_err(worker::Error::KvError)
        .inspect_err(|e| error!("Failed to get result from cache with error: {e}"))?;

    if let Some(response) = cache_response {
        Ok(Some(response))
    } else {
        let result = get_data_from_s3(query, bucket, env).await;

        if let Ok(Some(r)) = &result {
            let result_put_op = cache_store
                .put(&query_hash, r)
                .map_err(worker::Error::KvError)
                .inspect_err(|e| error!("Failed to build KV put request with error: {e}"))?;

            result_put_op
                .execute()
                .await
                .map_err(worker::Error::KvError)
                .inspect_err(|e| error!("Failed to execute KV put request with error: {e}"))?;
        };

        result
    }
}

async fn get_variant(request: Request, ctx: RouteContext<()>) -> worker::Result<Response> {
    let query: AlphamissenseQuery = request
        .query()
        .inspect_err(|e| error!("Failed to deserialize query with error: {e}"))?;

    // Check to see if it's cached
    let cache_store = ctx
        .env
        .kv("CACHE")
        .inspect_err(|e| error!("Failed to get cache KV store with error: {e}"))?;

    // Get bucket
    let bucket = ctx
        .bucket("BUCKET")
        .inspect_err(|e| error!("Failed to get bucket from context with error: {e}"))?;

    let result = get_result_from_cache_or_s3(&query, &cache_store, &bucket, &ctx.env).await;

    let response = match result {
        Ok(Some(r)) => Response::from_json(&r),
        Ok(None) => Response::from_json(&()),
        Err(e) => {
            error!("Failed to get alphamissense result with error: {e}");
            Err(worker::Error::Internal("500".into()))
        }
    };

    // Add CORS headers to response
    let cors = Cors::new().with_origins(["*"]);

    response.map(|r| r.with_cors(&cors))?
}

#[event(fetch)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> worker::Result<Response> {
    Router::new()
        .get_async("/v1/variant", get_variant)
        .run(req, env)
        .await
}
