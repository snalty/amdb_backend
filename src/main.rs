#[macro_use]
extern crate rocket;

use rocket::{
    http::Method,
    serde::{json::Json, Deserialize, Serialize},
};
use rocket_cors::{AllowedHeaders, AllowedOrigins};
use rocket_db_pools::{sqlx, Connection, Database};
use rocket_okapi::{
    okapi::schemars,
    openapi_get_routes,
    swagger_ui::{make_swagger_ui, SwaggerUIConfig}
};
use rocket_okapi::{okapi::schemars::JsonSchema, openapi};

#[derive(Database)]
#[database("sqlx")]
struct Db(sqlx::PgPool);

type Result<T, E = rocket::response::Debug<sqlx::Error>> = std::result::Result<T, E>;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
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

#[derive(FromForm, JsonSchema)]
struct AlphamissenseQuery {
    chrom: String,
    pos: i32,
    ref_allele: String,
    alt_allele: String,
    genome: String,
}
/// # Get variant at position
///
/// Returns variant at a given chromosome, position, reference allele and alternate allele and genome
#[openapi(tag = "Variants")]
#[get("/v1/variant?<query..>")]
async fn get_variant(
    mut db: Connection<Db>,
    query: AlphamissenseQuery,
) -> Result<Json<Option<AlphamissenseResult>>> {
    let results = sqlx::query_as!(
        AlphamissenseResult,
        "SELECT * FROM alphamissense 
        WHERE chrom = $1
        AND pos = $2
        AND ref = $3
        AND alt = $4
        AND genome = $5;",
        query.chrom,
        query.pos,
        query.ref_allele,
        query.alt_allele,
        query.genome
    )
    .fetch_optional(&mut **db)
    .await?;

    Ok(Json(results))
}

#[launch]
fn rocket() -> _ {
    let allowed_origins = AllowedOrigins::all();

    // You can also deserialize this
    let cors = rocket_cors::CorsOptions {
        allowed_origins,
        allowed_methods: vec![Method::Get].into_iter().map(From::from).collect(),
        allowed_headers: AllowedHeaders::some(&["Authorization", "Accept"]),
        allow_credentials: true,
        ..Default::default()
    }
    .to_cors()
    .unwrap();

    rocket::build()
        .mount("/", openapi_get_routes![get_variant])
        .mount(
            "/swagger-ui/",
            make_swagger_ui(&SwaggerUIConfig {
                url: "../openapi.json".to_owned(),
                ..Default::default()
            }),
        )
        .attach(cors)
        .attach(Db::init())
}
