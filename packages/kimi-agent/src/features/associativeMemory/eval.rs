use super::metacognition::{CalibrationBucket, CalibrationSample, calibrate};

#[derive(Debug, Clone, PartialEq)]
pub struct RecallEvaluation {
    pub queries: usize,
    pub hit_at1: f64,
    pub hit_at5: f64,
    pub mrr: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExtendedRecallEvaluation {
    pub queries: usize,
    pub hit_at1: f64,
    pub hit_at5: f64,
    pub mrr: f64,
    pub retention: f64,
    pub interference: f64,
    pub calibration: Vec<CalibrationBucket>,
    pub cost_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct EvaluationOptions {
    pub k: Option<usize>,
    pub retention: Option<f64>,
    pub interference: Option<f64>,
    pub calibration: Option<Vec<CalibrationBucket>>,
    pub cost_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RetentionSample {
    pub expected_id: String,
    pub recalled: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InterferenceSample {
    pub recalled: Vec<String>,
    pub distractors: Vec<String>,
}

pub fn evaluate_recall(
    results: &[Vec<String>],
    ground_truth: &[Vec<String>],
    k: usize,
) -> RecallEvaluation {
    let queries = results.len().min(ground_truth.len());
    let mut hit_at1 = 0usize;
    let mut hit_at5 = 0usize;
    let mut reciprocal = 0.0;
    for index in 0..queries {
        let ids = &results[index];
        let expected: std::collections::HashSet<&str> =
            ground_truth[index].iter().map(String::as_str).collect();
        let rank = ids
            .iter()
            .position(|id| expected.contains(id.as_str()));
        if rank == Some(0) {
            hit_at1 += 1;
        }
        if let Some(rank) = rank
            && rank < k
        {
            hit_at5 += 1;
        }
        if let Some(rank) = rank {
            reciprocal += 1.0 / (rank + 1) as f64;
        }
    }
    RecallEvaluation {
        queries,
        hit_at1: if queries == 0 {
            0.0
        } else {
            hit_at1 as f64 / queries as f64
        },
        hit_at5: if queries == 0 {
            0.0
        } else {
            hit_at5 as f64 / queries as f64
        },
        mrr: if queries == 0 {
            0.0
        } else {
            reciprocal / queries as f64
        },
    }
}

pub fn evaluate_extended(
    results: &[Vec<String>],
    ground_truth: &[Vec<String>],
    options: &EvaluationOptions,
) -> ExtendedRecallEvaluation {
    let recall = evaluate_recall(results, ground_truth, options.k.unwrap_or(5));
    ExtendedRecallEvaluation {
        queries: recall.queries,
        hit_at1: recall.hit_at1,
        hit_at5: recall.hit_at5,
        mrr: recall.mrr,
        retention: options.retention.unwrap_or(0.0),
        interference: options.interference.unwrap_or(0.0),
        calibration: options.calibration.clone().unwrap_or_default(),
        cost_ms: options.cost_ms.unwrap_or(0),
    }
}

pub fn retention_rate(samples: &[RetentionSample]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let kept = samples
        .iter()
        .filter(|sample| sample.recalled.iter().any(|id| id == &sample.expected_id))
        .count();
    kept as f64 / samples.len() as f64
}

pub fn interference_rate(samples: &[InterferenceSample]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let contaminated = samples
        .iter()
        .filter(|sample| {
            let distractors: std::collections::HashSet<&str> =
                sample.distractors.iter().map(String::as_str).collect();
            sample
                .recalled
                .iter()
                .any(|id| distractors.contains(id.as_str()))
        })
        .count();
    contaminated as f64 / samples.len() as f64
}

pub fn collect_cost_ms(started_at: i64, ended_at: i64) -> i64 {
    (ended_at - started_at).max(0)
}

pub fn collect_calibration(
    samples: &[CalibrationSample],
    buckets: usize,
) -> Vec<CalibrationBucket> {
    calibrate(samples, buckets)
}

pub fn format_evaluation(evaluation: &RecallEvaluation) -> String {
    format!(
        "queries: {}  hit@1: {}  hit@5: {}  mrr:   {}",
        evaluation.queries,
        js_to_fixed(evaluation.hit_at1),
        js_to_fixed(evaluation.hit_at5),
        js_to_fixed(evaluation.mrr)
    )
}

fn js_to_fixed(value: f64) -> String {
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| ctx.eval::<String, _>(format!("({value}).toFixed(3)")))
            })
        })
        .unwrap_or_else(|_| format!("{value:.3}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_hits_are_ratios_over_the_shorter_side() {
        let results = vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["x".to_string()],
            vec!["y".to_string(), "a".to_string()],
        ];
        let truth = vec![
            vec!["a".to_string()],
            vec!["a".to_string()],
            vec!["a".to_string()],
        ];
        let evaluation = evaluate_recall(&results, &truth, 5);
        assert_eq!(evaluation.queries, 3);
        assert!((evaluation.hit_at1 - 1.0 / 3.0).abs() < 1e-12);
        assert!((evaluation.hit_at5 - 2.0 / 3.0).abs() < 1e-12);
        assert!((evaluation.mrr - (1.0 + 1.0 / 2.0) / 3.0).abs() < 1e-12);
    }

    #[test]
    fn a_k_smaller_than_the_rank_excludes_the_hit() {
        let results = vec![vec!["z".to_string(), "a".to_string()]];
        let truth = vec![vec!["a".to_string()]];
        let evaluation = evaluate_recall(&results, &truth, 1);
        assert_eq!(evaluation.hit_at1, 0.0);
        assert_eq!(evaluation.hit_at5, 0.0);
        assert_eq!(evaluation.mrr, 0.5);
    }

    #[test]
    fn empty_inputs_are_all_zero() {
        let evaluation = evaluate_recall(&[], &[], 5);
        assert_eq!(evaluation.queries, 0);
        assert_eq!(evaluation.hit_at1, 0.0);
        assert_eq!(evaluation.hit_at5, 0.0);
        assert_eq!(evaluation.mrr, 0.0);
        assert_eq!(format_evaluation(&evaluation), "queries: 0  hit@1: 0.000  hit@5: 0.000  mrr:   0.000");
    }

    #[test]
    fn extended_evaluation_carries_the_option_defaults() {
        let extended = evaluate_extended(&[], &[], &EvaluationOptions::default());
        assert_eq!(extended.queries, 0);
        assert_eq!(extended.retention, 0.0);
        assert_eq!(extended.interference, 0.0);
        assert!(extended.calibration.is_empty());
        assert_eq!(extended.cost_ms, 0);

        let buckets = collect_calibration(
            &[CalibrationSample {
                confidence: 1.0,
                confirmed: true,
            }],
            5,
        );
        let extended = evaluate_extended(
            &[],
            &[],
            &EvaluationOptions {
                k: Some(1),
                retention: Some(0.75),
                interference: Some(0.25),
                calibration: Some(buckets.clone()),
                cost_ms: Some(12),
            },
        );
        assert_eq!(extended.retention, 0.75);
        assert_eq!(extended.interference, 0.25);
        assert_eq!(extended.cost_ms, 12);
        assert_eq!(extended.calibration.len(), 5);
        assert_eq!(extended.calibration[4].count, 1);
    }

    #[test]
    fn retention_and_interference_are_hit_ratios() {
        let retention = retention_rate(&[
            RetentionSample {
                expected_id: "a".into(),
                recalled: vec!["a".into()],
            },
            RetentionSample {
                expected_id: "b".into(),
                recalled: vec!["x".into()],
            },
        ]);
        assert_eq!(retention, 0.5);
        assert_eq!(retention_rate(&[]), 0.0);

        let interference = interference_rate(&[
            InterferenceSample {
                recalled: vec!["a".into(), "d1".into()],
                distractors: vec!["d1".into()],
            },
            InterferenceSample {
                recalled: vec!["a".into()],
                distractors: vec!["d1".into()],
            },
        ]);
        assert_eq!(interference, 0.5);
        assert_eq!(interference_rate(&[]), 0.0);
    }

    #[test]
    fn cost_is_clamped_at_zero() {
        assert_eq!(collect_cost_ms(10, 4), 0);
        assert_eq!(collect_cost_ms(4, 10), 6);
    }
}
