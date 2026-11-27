//! UI blocks (cards, charts, tables, stat tiles, diagrams) checked before an
//! app renders them (feature `ui`). Without the feature, [`validate_ui_block`]
//! reports that it is unavailable and runs emit no `Ui` events.

use crate::GraphrtError;
use crate::events::UiBlock;

/// The checked block, or why it must not be shown.
#[cfg(feature = "ui")]
pub(crate) fn checked(json: &serde_json::Value) -> Result<UiBlock, String> {
    let block = graphrt_ui::validate(json).map_err(|e| e.to_string())?;
    Ok(UiBlock {
        kind: block.kind().as_str().to_owned(),
        title: block.title().map(str::to_owned),
        json: block.as_json().to_string(),
    })
}

/// Check a UI block (`{"$ui": "chart", "version": 1, ...}`) against its schema,
/// for blocks an app receives outside a run (stored messages, a server).
#[uniffi::export]
pub fn validate_ui_block(block_json: String) -> Result<UiBlock, GraphrtError> {
    #[cfg(feature = "ui")]
    {
        let json: serde_json::Value =
            serde_json::from_str(&block_json).map_err(|e| GraphrtError::input(format!("block: {e}")))?;
        checked(&json).map_err(GraphrtError::input)
    }
    #[cfg(not(feature = "ui"))]
    {
        let _ = block_json;
        Err(GraphrtError::unsupported("UI block validation", "ui"))
    }
}

#[cfg(all(test, feature = "ui"))]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_checked() {
        let stats = r#"{"$ui": "stats", "version": 1, "title": "Today", "items": [{"label": "Orders", "value": 12}]}"#;
        let block = validate_ui_block(stats.into()).unwrap();
        assert_eq!((block.kind.as_str(), block.title.as_deref()), ("stats", Some("Today")));
        assert!(validate_ui_block(r#"{"$ui": "stats", "version": 2}"#.into()).is_err());
        assert!(validate_ui_block(r#"{"$ui": "slideshow", "version": 1}"#.into()).is_err());
        assert!(validate_ui_block("[".into()).is_err());
    }
}
