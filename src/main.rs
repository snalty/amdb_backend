#[macro_use] extern crate rocket;
#[macro_use] extern crate rocket_db_pools;

use rocket::{serde::{Serialize, Deserialize, json::Json}, form::Form, futures::TryStreamExt, http::Method};
use rocket_cors::{AllowedOrigins, AllowedHeaders};
use rocket_db_pools::{sqlx, Database, Connection};

#[derive(Database)]
#[database("sqlx")]
struct Db(sqlx::PgPool);

type Result<T, E = rocket::response::Debug<sqlx::Error>> = std::result::Result<T, E>;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(crate = "rocket::serde")]
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

#[derive(FromForm)]
struct AlphamissenseQuery {
    chrom: String,
    pos: i32,
    ref_allele: Option<String>,
    alt_allele: Option<String>,
    genome: String,
}

#[get("/variant?<query..>")]
async fn get_variant(mut db: Connection<Db>, query: AlphamissenseQuery) -> Result<Json<Vec<AlphamissenseResult>>> {
    let results = sqlx::query_as!(
        AlphamissenseResult,
        "SELECT * FROM alphamissense 
        WHERE chrom = $1
        AND pos = $2
        AND ref = $3
        AND alt = $4
        AND genome = $5;", query.chrom, query.pos, query.ref_allele, query.alt_allele, query.genome
    ).fetch(&mut **db)
    .try_collect::<Vec<_>>()
    .await?;

    Ok(Json(results))
}

#[launch]
fn rocket() -> _ {
    let allowed_origins = AllowedOrigins::some_exact(&["http://localhost:5173"]);

    // You can also deserialize this
    let cors = rocket_cors::CorsOptions {
        allowed_origins,
        allowed_methods: vec![Method::Get].into_iter().map(From::from).collect(),
        allowed_headers: AllowedHeaders::some(&["Authorization", "Accept"]),
        allow_credentials: true,
        ..Default::default()
    }
    .to_cors().unwrap();

    rocket::build().mount("/", routes![get_variant]).attach(cors).attach(Db::init())
}