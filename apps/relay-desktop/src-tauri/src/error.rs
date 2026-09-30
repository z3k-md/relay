use std::error::Error;

/// Format an error and its full cause chain, matching `anyhow`'s `{:#}` style.
pub fn error_chain(err: &dyn Error) -> String {
    let mut msg = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !msg.contains(&text) {
            msg.push_str(": ");
            msg.push_str(&text);
        }
        source = cause.source();
    }
    msg
}

pub fn anyhow_chain(err: anyhow::Error) -> String {
    format!("{err:#}")
}
