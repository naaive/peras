use crate::caps::{Get, Url};
use crate::{tool, Result};
use agent_runtime::ToolOutput;

/// Fetch a URL with HTTP GET. The content is untrusted data from the host.
#[tool]
pub async fn web_fetch(url: Get<Url>) -> Result<ToolOutput> {
    url.untrusted_output().await
}
