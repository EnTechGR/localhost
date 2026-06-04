/// MIME type resolution from file extensions.
///
/// Returns the MIME type string for a given extension. Extensions are matched
/// case-insensitively. Unknown extensions fall back to
/// `application/octet-stream`.

/// Resolve the MIME type for a file path based on its extension.
///
/// The path may contain directory components; only the final extension is
/// examined. Returns `"application/octet-stream"` if the extension is
/// unknown or absent.
pub fn from_path(path: &str) -> &'static str {
    let ext = path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_lowercase();
    from_extension(&ext)
}

/// Resolve the MIME type from a lowercase extension string (without the dot).
pub fn from_extension(ext: &str) -> &'static str {
    match ext {
        // Text
        "html" | "htm"  => "text/html; charset=utf-8",
        "css"            => "text/css; charset=utf-8",
        "js" | "mjs"     => "application/javascript; charset=utf-8",
        "json"           => "application/json",
        "txt"            => "text/plain; charset=utf-8",
        "md"             => "text/markdown; charset=utf-8",
        "csv"            => "text/csv; charset=utf-8",
        "xml"            => "application/xml",
        "svg"            => "image/svg+xml",

        // Images
        "png"            => "image/png",
        "jpg" | "jpeg"   => "image/jpeg",
        "gif"            => "image/gif",
        "webp"           => "image/webp",
        "ico"            => "image/x-icon",
        "bmp"            => "image/bmp",
        "tiff" | "tif"   => "image/tiff",
        "avif"           => "image/avif",

        // Fonts
        "woff"           => "font/woff",
        "woff2"          => "font/woff2",
        "ttf"            => "font/ttf",
        "otf"            => "font/otf",

        // Audio / Video
        "mp3"            => "audio/mpeg",
        "ogg"            => "audio/ogg",
        "wav"            => "audio/wav",
        "mp4"            => "video/mp4",
        "webm"           => "video/webm",
        "ogv"            => "video/ogg",

        // Archives / Binary
        "pdf"            => "application/pdf",
        "zip"            => "application/zip",
        "gz" | "tgz"     => "application/gzip",
        "tar"            => "application/x-tar",
        "wasm"           => "application/wasm",

        // Data
        "atom"           => "application/atom+xml",
        "rss"            => "application/rss+xml",

        // Default
        _               => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_mime() {
        assert_eq!(from_path("/index.html"), "text/html; charset=utf-8");
        assert_eq!(from_path("/page.htm"),   "text/html; charset=utf-8");
    }

    #[test]
    fn css_js_json() {
        assert_eq!(from_path("/style.css"),    "text/css; charset=utf-8");
        assert_eq!(from_path("/app.js"),       "application/javascript; charset=utf-8");
        assert_eq!(from_path("/data.json"),    "application/json");
    }

    #[test]
    fn image_types() {
        assert_eq!(from_path("/logo.png"),  "image/png");
        assert_eq!(from_path("/photo.jpg"), "image/jpeg");
        assert_eq!(from_path("/icon.ico"),  "image/x-icon");
    }

    #[test]
    fn unknown_extension_fallback() {
        assert_eq!(from_path("/file.xyz"),  "application/octet-stream");
        assert_eq!(from_path("/file"),      "application/octet-stream");
    }

    #[test]
    fn path_with_directory_components() {
        assert_eq!(from_path("/static/css/app.css"), "text/css; charset=utf-8");
        assert_eq!(from_path("/a/b/c/d.json"),       "application/json");
    }

    #[test]
    fn wasm_type() {
        assert_eq!(from_path("/module.wasm"), "application/wasm");
    }
}