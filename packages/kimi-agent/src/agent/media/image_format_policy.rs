pub use crate::llm::image_policy::*;

pub fn build_image_conversion_guidance(path: &str, mime_type: &str, os_kind: Option<&str>) -> String {
    let converted = converted_path(path);
    format!(
        "\"{path}\" is an {mime_type} image, which the provider does not accept. Convert it to JPEG first, then read the converted file. {}",
        image_conversion_command(path, &converted, mime_type, os_kind)
    )
}

pub fn build_oversized_image_conversion_guidance(
    path: &str,
    mime_type: &str,
    os_kind: &str,
    byte_length: u64,
    inline_byte_budget: u64,
) -> String {
    let converted = converted_path(path);
    format!(
        "\"{path}\" is a {byte_length}-byte {mime_type} image, over the {inline_byte_budget}-byte per-image limit, and this format cannot be downsampled locally. Convert it to JPEG first, then read the converted file. {}",
        image_conversion_command(path, &converted, mime_type, Some(os_kind))
    )
}

fn converted_path(path: &str) -> String {
    let last_separator = path.rfind(['/', '\\']).map_or(0, |index| index + 1);
    match path
        .rfind('.')
        .filter(|index| *index >= last_separator && *index + 1 < path.len())
    {
        Some(index) => format!("{}.jpg", &path[..index]),
        None => format!("{path}.jpg"),
    }
}

fn image_conversion_command(
    path: &str,
    converted: &str,
    mime_type: &str,
    os_kind: Option<&str>,
) -> String {
    let magick = format!("magick \"{path}\" \"{converted}\"");
    let decoder = matches!(
        normalize_image_mime(mime_type).as_str(),
        "image/heic" | "image/heif"
    )
    .then_some(("heif-convert", "libheif-examples"));
    match os_kind {
        Some("macOS") => {
            format!("On macOS: sips -s format jpeg \"{path}\" --out \"{converted}\"")
        }
        Some("Linux") => match decoder {
            Some((command, package_name)) => format!(
                "On Linux: {command} \"{path}\" \"{converted}\" (package {package_name}), or with ImageMagick: {magick}"
            ),
            None => format!("On Linux, with ImageMagick: {magick}"),
        },
        Some("Windows") => format!(
            "On Windows, with ImageMagick: {magick} (install it first if missing: winget install ImageMagick.ImageMagick)"
        ),
        _ => {
            let decoder = decoder
                .map(|(command, package_name)| {
                    format!(
                        ", {command} \"{path}\" \"{converted}\" (Linux, package {package_name})"
                    )
                })
                .unwrap_or_default();
            format!(
                "Options: sips -s format jpeg \"{path}\" --out \"{converted}\" (macOS){decoder}, or {magick} (ImageMagick)"
            )
        }
    }
}
