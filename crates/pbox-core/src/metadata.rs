use crate::PboxId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MARKER_PREFIX: &str = "<!-- pbox-meta-v1";
const MARKER_SUFFIX: &str = "-->";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PboxMetadata {
    pub id: PboxId,
    pub vmid: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

impl PboxMetadata {
    pub fn new(id: PboxId, vmid: u64) -> Self {
        Self {
            id,
            vmid,
            node: None,
        }
    }

    pub fn with_node(mut self, node: impl Into<String>) -> Self {
        self.node = Some(node.into());
        self
    }
}

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("metadata marker is missing its closing -->")]
    UnterminatedMarker,
    #[error("metadata marker must be unique")]
    MultipleMarkers,
    #[error("invalid pbox-meta-v1 JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

pub fn encode_metadata(metadata: &PboxMetadata) -> Result<String, MetadataError> {
    let json = serde_json::to_string(metadata)?;
    Ok(format!("{MARKER_PREFIX} {json} {MARKER_SUFFIX}"))
}

pub fn parse_metadata(markdown: &str) -> Result<Option<PboxMetadata>, MetadataError> {
    let spans = marker_spans(markdown)?;
    match spans.as_slice() {
        [] => Ok(None),
        [span] => parse_span(markdown, *span).map(Some),
        _ => Err(MetadataError::MultipleMarkers),
    }
}

pub fn preserve_metadata(markdown: &str, metadata: &PboxMetadata) -> Result<String, MetadataError> {
    let marker = encode_metadata(metadata)?;
    let spans = marker_spans(markdown)?;
    if spans.is_empty() {
        let mut output = markdown.to_owned();
        if !output.is_empty() {
            if !output.ends_with('\n') {
                output.push_str("\n\n");
            } else {
                output.push('\n');
            }
        }
        output.push_str(&marker);
        return Ok(output);
    }

    // Replace the first block in place and remove any stale duplicates. Text outside
    // marker spans is copied byte-for-byte, so user-authored Markdown is preserved.
    let mut output = String::with_capacity(markdown.len() + marker.len());
    let mut cursor = 0;
    for (index, (start, end)) in spans.iter().copied().enumerate() {
        output.push_str(&markdown[cursor..start]);
        if index == 0 {
            output.push_str(&marker);
        }
        cursor = end;
    }
    output.push_str(&markdown[cursor..]);
    Ok(output)
}

fn marker_spans(markdown: &str) -> Result<Vec<(usize, usize)>, MetadataError> {
    let mut spans = Vec::new();
    let mut search_from = 0;
    while let Some(relative_start) = markdown[search_from..].find(MARKER_PREFIX) {
        let start = search_from + relative_start;
        let relative_end = markdown[start..]
            .find(MARKER_SUFFIX)
            .ok_or(MetadataError::UnterminatedMarker)?;
        let end = start + relative_end + MARKER_SUFFIX.len();
        spans.push((start, end));
        search_from = end;
    }
    Ok(spans)
}

fn parse_span(markdown: &str, (start, end): (usize, usize)) -> Result<PboxMetadata, MetadataError> {
    let marker = &markdown[start..end];
    let json = marker
        .strip_prefix(MARKER_PREFIX)
        .and_then(|value| value.strip_suffix(MARKER_SUFFIX))
        .map(str::trim)
        .unwrap_or_default();
    Ok(serde_json::from_str(json)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_round_trips_and_preserves_notes() {
        let metadata =
            PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9000).with_node("pve-a");
        let markdown = "User note\n\nKeep this text.";
        let encoded = preserve_metadata(markdown, &metadata).unwrap();
        assert!(encoded.contains("User note"));
        assert_eq!(parse_metadata(&encoded).unwrap(), Some(metadata));
    }

    #[test]
    fn stale_duplicate_markers_are_collapsed() {
        let first = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9000);
        let second = PboxMetadata::new(PboxId::parse("pbx_12345678").unwrap(), 9001);
        let source = format!(
            "note\n{}\n{}",
            encode_metadata(&first).unwrap(),
            encode_metadata(&first).unwrap()
        );
        let result = preserve_metadata(&source, &second).unwrap();
        assert_eq!(result.matches(MARKER_PREFIX).count(), 1);
        assert_eq!(parse_metadata(&result).unwrap(), Some(second));
    }
}
