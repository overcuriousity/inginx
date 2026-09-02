pub mod auth;

use crate::proxy::AppState;
use axum::Router;

pub fn router(state: AppState) -> Router {
    auth::router(state)
}
