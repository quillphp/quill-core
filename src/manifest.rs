use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct RouteEntry {
    pub method: String,
    pub pattern: String,
    pub handler_id: u32,
    pub dto_class: Option<String>,
    pub max_body_size: Option<usize>,
}
