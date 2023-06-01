// Stract is an open source web search engine.
// Copyright (C) 2023 Stract ApS
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

use std::collections::HashMap;

use super::{
    optics::{OpticLink, DEFAULT_OPTICS},
    HtmlTemplate,
};
use askama::Template;
use axum::{extract, response::IntoResponse};

#[allow(clippy::unused_async)]
pub async fn route(
    extract::Query(params): extract::Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let template = ChatTemplate {
        query_url_part: serde_urlencoded::to_string(params).unwrap(),
        default_optics: DEFAULT_OPTICS.to_vec(),
    };
    HtmlTemplate(template)
}

#[derive(Template)]
#[template(path = "chat/index.html")]
struct ChatTemplate {
    query_url_part: String,
    default_optics: Vec<OpticLink>,
}
