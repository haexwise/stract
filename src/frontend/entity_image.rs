// Cuely is an open source web search engine.
// Copyright (C) 2022 Cuely ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::Path;
use axum::http::Response;
use axum::response::IntoResponse;
use axum::Extension;
use reqwest::StatusCode;

use super::State;

pub async fn route(
    Path(entity): Path<String>,
    Extension(state): Extension<Arc<State>>,
) -> impl IntoResponse {
    let img = state.searcher.entity_image(entity);

    let bytes = match img {
        Some(img) => img.as_raw_bytes(),
        None => Vec::new(),
    };

    Response::builder()
        .status(StatusCode::OK)
        .body(Body::from(bytes))
        .unwrap()
}