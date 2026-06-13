// Source-metadata endpoints. Currently just the Open-Meteo forecast
// model catalog: GET /api/v1/sources/openmeteo/models returns every
// model id `sources[].config.model` accepts (id, label, agency,
// region), so a settings UI can render the picker without hardcoding
// the list. Static data straight from forecast::model_catalog; shape
// locked by the openmeteo_models_v1 snapshot test.

use axum::{response::Json, routing::get, Router};

use crate::forecast::model_catalog::{models, ForecastModel};

pub fn router() -> Router {
    Router::new().route("/sources/openmeteo/models", get(openmeteo_models))
}

async fn openmeteo_models() -> Json<&'static [ForecastModel]> {
    Json(models())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn models_endpoint_returns_catalog_with_best_match_first() {
        let Json(body) = openmeteo_models().await;
        assert_eq!(body[0].id, "best_match");
        assert_eq!(body.len(), crate::forecast::model_catalog::models().len());
    }
}
