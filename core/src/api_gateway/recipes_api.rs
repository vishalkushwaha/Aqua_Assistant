//! The recipes Aqua has learned, for the person to see and to forget (card R2-13, task T2-13.3): `GET /api/v1/learning/recipes` and
//! `DELETE /api/v1/learning/recipes/{id}`. Both need the UI token: a recipe is made from what the person asked, and forgetting one is a change to what Aqua knows.
//!
//! * **List.** Every recipe as it is stored: its goal (personal details removed; a recipe that takes values from a goal shows `{{file1}}` and the like in their places),
//!   the number of steps, the version (1 for a recipe made before versions, 2 for one with placeholders and postconditions), how often and when it was used, how often it
//!   worked, whether any step changes something, and whether it was learned in a run that had read text from outside. **No step, argument or value is shown.**
//! * **Forget.** One recipe by its id. The database overwrites what it deletes. A recipe that is gone is a 404.
//! * **No more.** There is no route that makes a recipe or changes one: a recipe is made only by Aqua, from a run that went well.
//!
//! The routes take [`RecipesState`], the folder the recipes are kept under, and not the whole gateway state (which a test cannot build), so that a route test can build the
//! router over a folder of its own (see `core/tests/recipes_api_test.rs`).

use super::auth::{ui_scope_required, Scope};
use super::server::{api_error, AppState};
use crate::memory::agent_learning::{self, recipes_v2_enabled};
use crate::memory::SolutionsDb;
use axum::{
    extract::{FromRef, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get},
    Json, Router,
};
use serde_json::json;
use std::path::PathBuf;

/// Where the recipes are kept: the workspace folder, whose `data/solutions.db` holds them. The gateway's [`AppState`] gives it (`FromRef`); a test builds it over a folder of its own.
#[derive(Clone)]
pub struct RecipesState {
    pub root: PathBuf,
}

impl FromRef<AppState> for RecipesState {
    fn from_ref(_: &AppState) -> Self {
        Self { root: crate::runtime::workspace_root() }
    }
}

/// The two routes.
pub fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    RecipesState: FromRef<S>,
{
    Router::new().route("/api/v1/learning/recipes", get(list_recipes)).route("/api/v1/learning/recipes/:id", delete(delete_recipe))
}

/// Lists the learned recipes: goal, steps, version, uses, last use, success rate, whether any step changes something. No step or value is shown. Needs the UI token.
async fn list_recipes(State(state): State<RecipesState>, axum::Extension(scope): axum::Extension<Scope>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let root = state.root.clone();
    let rows = tokio::task::spawn_blocking(move || SolutionsDb::new(&root).list_solutions()).await;
    match rows {
        Ok(Ok(rows)) => {
            let recipes: Vec<_> = rows.iter().map(agent_learning::summarize).collect();
            Json(json!({ "recipes": recipes, "count": recipes.len(), "recipes_v2": recipes_v2_enabled() })).into_response()
        }
        _ => api_error(StatusCode::INTERNAL_SERVER_ERROR, "the learned recipes could not be read"),
    }
}

/// Forgets one learned recipe by its id (404 when there is none). Needs the UI token.
async fn delete_recipe(State(state): State<RecipesState>, axum::Extension(scope): axum::Extension<Scope>, Path(id): Path<i64>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let root = state.root.clone();
    match tokio::task::spawn_blocking(move || SolutionsDb::new(&root).delete_solution(id)).await {
        Ok(Ok(true)) => Json(json!({ "deleted": true, "id": id })).into_response(),
        Ok(Ok(false)) => api_error(StatusCode::NOT_FOUND, "no learned recipe has that id"),
        _ => api_error(StatusCode::INTERNAL_SERVER_ERROR, "the recipe could not be forgotten"),
    }
}
