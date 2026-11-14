use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerReviewToolInput {
    pub target: String,
    pub status: String,
    pub merge: String,
    pub findings: String,
    pub checks: Option<Vec<String>>,
    pub decision: String,
    pub acceptance_tip: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TowerReviewToolInputWire {
    target: String,
    status: String,
    merge: String,
    findings: String,
    #[serde(default, deserialize_with = "deserialize_optional")]
    checks: Option<Vec<String>>,
    decision: String,
    #[serde(default, deserialize_with = "deserialize_optional")]
    acceptance_tip: Option<String>,
}

impl<'de> Deserialize<'de> for TowerReviewToolInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = TowerReviewToolInputWire::deserialize(deserializer)?;
        if !crate::tools::regex_js::is_match_with_flags(
            r"^(clean|p[12]-\d+items)$",
            &wire.status,
            "",
        )
        .unwrap_or(false)
        {
            return Err(serde::de::Error::custom("invalid review status"));
        }
        if !matches!(wire.merge.as_str(), "merge" | "fix-then-merge" | "hold") {
            return Err(serde::de::Error::custom("invalid review merge verdict"));
        }
        Ok(Self {
            target: wire.target,
            status: wire.status,
            merge: wire.merge,
            findings: wire.findings,
            checks: wire.checks,
            decision: wire.decision,
            acceptance_tip: wire.acceptance_tip,
        })
    }
}

fn deserialize_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub const TOWER_REVIEW_TOOL_SERVICE_ID: &str = "towerReviewTool";

pub fn tower_review_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["target", "status", "merge", "findings", "decision"],
        "properties": {
            "target": {
                "type": "string",
                "description": "The branch you were assigned to review"
            },
            "status": {
                "type": "string",
                "pattern": "^(clean|p[12]-\\d+items)$",
                "description": "Verdict: \"clean\", or \"p1-Nitems\" / \"p2-Nitems\" with the number of findings at that priority"
            },
            "merge": {
                "type": "string",
                "enum": ["merge", "fix-then-merge", "hold"],
                "description": "Merge recommendation for the tower"
            },
            "findings": {
                "type": "string",
                "description": "Full findings text (markdown); write \"none\" when clean"
            },
            "checks": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Checklist items you verified (e.g. \"tests pass\", \"no secrets\")"
            },
            "decision": {
                "type": "string",
                "description": "The reasoning behind your verdict"
            },
            "acceptance_tip": {
                "type": "string",
                "description": "Required for tower-recorded artifact acceptance: the completion tip shown in the acceptance-request the human approved"
            }
        },
        "additionalProperties": false
    })
}
