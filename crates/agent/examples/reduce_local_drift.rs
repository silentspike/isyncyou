use isyncyou_agent::drift_capture::{
    reduce_capture, write_summary_atomic, CaptureProvider, DriftCaptureOptions,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn parse_args() -> Result<(DriftCaptureOptions, PathBuf), &'static str> {
    let mut args = std::env::args().skip(1);
    let mut values = BTreeMap::new();
    while let Some(flag) = args.next() {
        if !matches!(
            flag.as_str(),
            "--provider"
                | "--version-file"
                | "--event-file"
                | "--debug-file"
                | "--model-catalog"
                | "--bundled-model-catalog"
                | "--expected-sentinel"
                | "--output"
        ) {
            return Err("unknown argument");
        }
        let value = args.next().ok_or("missing argument value")?;
        if values.insert(flag, value).is_some() {
            return Err("duplicate argument");
        }
    }

    let required = |flag: &str| values.get(flag).cloned().ok_or("missing argument");
    let provider =
        CaptureProvider::parse(&required("--provider")?).ok_or("unsupported provider")?;
    let output = PathBuf::from(required("--output")?);
    Ok((
        DriftCaptureOptions {
            provider,
            version_file: PathBuf::from(required("--version-file")?),
            event_file: PathBuf::from(required("--event-file")?),
            debug_file: values.get("--debug-file").map(PathBuf::from),
            model_catalog: values.get("--model-catalog").map(PathBuf::from),
            bundled_model_catalog: values.get("--bundled-model-catalog").map(PathBuf::from),
            expected_sentinel: required("--expected-sentinel")?,
        },
        output,
    ))
}

fn main() {
    let result = parse_args()
        .map_err(str::to_string)
        .and_then(|(options, output)| {
            let summary = reduce_capture(&options).map_err(|error| error.to_string())?;
            let review = summary.manual_review_categories();
            if !review.is_empty() {
                eprintln!("local drift manual review categories: {}", review.join(","));
            }
            write_summary_atomic(
                options.provider,
                &summary,
                &options.expected_sentinel,
                &output,
            )
            .map_err(|error| error.to_string())
        });
    if let Err(error) = result {
        eprintln!("local drift reduction failed: {error}");
        std::process::exit(2);
    }
}
