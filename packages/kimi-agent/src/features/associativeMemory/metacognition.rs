#[derive(Debug, Clone, PartialEq)]
pub struct CalibrationBucket {
    pub bucket: usize,
    pub predicted: f64,
    pub observed: f64,
    pub count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CalibrationSample {
    pub confidence: f64,
    pub confirmed: bool,
}

pub fn update_confidence(current: f64, confirmed: bool, rate: f64) -> f64 {
    let target = if confirmed { 1.0 } else { 0.0 };
    (current + rate * (target - current)).clamp(0.0, 1.0)
}

pub fn calibrate(samples: &[CalibrationSample], buckets: usize) -> Vec<CalibrationBucket> {
    if buckets == 0 {
        if samples.is_empty() {
            return Vec::new();
        }
        panic!("Cannot calibrate with zero buckets");
    }
    let mut predicted = vec![0.0; buckets];
    let mut observed = vec![0.0; buckets];
    let mut counts = vec![0usize; buckets];
    for sample in samples {
        assert!(!sample.confidence.is_nan(), "Cannot calibrate NaN confidence");
        let index = ((sample.confidence * buckets as f64).floor() as usize)
            .clamp(0, buckets - 1);
        predicted[index] += sample.confidence;
        observed[index] += if sample.confirmed { 1.0 } else { 0.0 };
        counts[index] += 1;
    }
    (0..buckets)
        .map(|bucket| CalibrationBucket {
            bucket,
            predicted: if counts[bucket] == 0 {
                0.0
            } else {
                predicted[bucket] / counts[bucket] as f64
            },
            observed: if counts[bucket] == 0 {
                0.0
            } else {
                observed[bucket] / counts[bucket] as f64
            },
            count: counts[bucket],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_moves_toward_the_outcome_and_clamps() {
        assert_eq!(update_confidence(0.5, true, 0.2), 0.6);
        assert_eq!(update_confidence(0.5, false, 0.2), 0.4);
        assert_eq!(update_confidence(0.95, true, 0.5), 0.975);
        assert_eq!(update_confidence(0.05, false, 0.5), 0.025);
        assert_eq!(update_confidence(0.5, true, 2.0), 1.0, "an over-unity rate clamps at the top");
        assert_eq!(update_confidence(0.5, false, 2.0), 0.0, "an over-unity rate clamps at the bottom");
        assert_eq!(update_confidence(0.5, true, 0.0), 0.5);
    }

    #[test]
    fn calibration_groups_by_confidence_band() {
        let samples = vec![
            CalibrationSample {
                confidence: 0.1,
                confirmed: true,
            },
            CalibrationSample {
                confidence: 0.9,
                confirmed: false,
            },
            CalibrationSample {
                confidence: 1.0,
                confirmed: true,
            },
        ];
        let buckets = calibrate(&samples, 5);
        assert_eq!(buckets.len(), 5);
        assert_eq!(buckets[0].count, 1);
        assert_eq!(buckets[0].predicted, 0.1);
        assert_eq!(buckets[0].observed, 1.0);
        assert_eq!(buckets[4].count, 2);
        assert_eq!(buckets[4].predicted, 0.95);
        assert_eq!(buckets[4].observed, 0.5);
        assert_eq!(buckets[2].count, 0);
        assert_eq!(buckets[2].predicted, 0.0);
        assert_eq!(buckets[2].observed, 0.0);
    }

    #[test]
    fn an_out_of_range_confidence_clamps_into_the_edge_bands() {
        let samples = vec![
            CalibrationSample {
                confidence: -5.0,
                confirmed: true,
            },
            CalibrationSample {
                confidence: 5.0,
                confirmed: false,
            },
        ];
        let buckets = calibrate(&samples, 2);
        assert_eq!(buckets[0].count, 1);
        assert_eq!(buckets[1].count, 1);
        assert!(calibrate(&[], 5).iter().all(|bucket| bucket.count == 0));
    }
}
