use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    emit_build_stamp();
    println!("cargo:rerun-if-changed=remote-client/dist/client");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist_dir = manifest_dir.join("remote-client/dist/client");
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("remote_assets.rs");

    if !dist_dir.join("index.html").is_file() {
        panic!(
            "remote client assets are missing; run `just remote-client-build` before building crabcode"
        );
    }

    let mut files = Vec::new();
    collect_files(&dist_dir, &dist_dir, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut output = String::new();
    output.push_str(
        "pub struct RemoteAsset {\n    pub content_type: &'static str,\n    pub body: &'static [u8],\n}\n\n",
    );
    output.push_str("pub fn remote_asset(path: &str) -> Option<RemoteAsset> {\n");
    output.push_str("    match path {\n");

    if files.is_empty() {
        output.push_str("        _ => None,\n");
    } else {
        for (route, file_path) in files {
            let route = route.replace('\\', "/");
            let content_type = content_type_for_path(&route);
            let path_literal = file_path.display().to_string().replace('\\', "\\\\");
            output.push_str(&format!(
                "        {:?} => Some(RemoteAsset {{ content_type: {:?}, body: include_bytes!({:?}) }}),\n",
                route, content_type, path_literal
            ));

            if route == "/index.html" {
                output.push_str(&format!(
                    "        \"/\" => Some(RemoteAsset {{ content_type: {:?}, body: include_bytes!({:?}) }}),\n",
                    content_type, path_literal
                ));
            }
        }
        output.push_str("        _ => None,\n");
    }

    output.push_str("    }\n}\n");

    fs::write(out_path, output).unwrap();
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }

        if path.is_dir() {
            collect_files(root, &path, files);
        } else if path.is_file() {
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let route = format!("/{}", relative.display());
            files.push((route, path));
        }
    }
}

fn content_type_for_path(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json; charset=utf-8",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn emit_build_stamp() {
    println!("cargo:rerun-if-env-changed=CRABCODE_BUILD_DATE");
    let stamp = env::var("CRABCODE_BUILD_DATE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            epoch_to_utc_string(now)
        });
    println!("cargo:rustc-env=CRABCODE_BUILD_STAMP={stamp}");
}

fn epoch_to_utc_string(secs: u64) -> String {
    let days = secs / 86400;
    let rem = secs % 86400;
    let hours = rem / 3600;
    let minutes = (rem % 3600) / 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}Z", year, month, day, hours, minutes)
}

fn days_to_ymd(mut days: u64) -> (u32, u32, u32) {
    let mut year = 1970;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
        let year_days = if leap { 366 } else { 365 };
        if days >= year_days {
            days -= year_days;
            year += 1;
        } else {
            break;
        }
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
    let months = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 1;
    for &m_days in &months {
        if days >= m_days {
            days -= m_days;
            month += 1;
        } else {
            break;
        }
    }
    let day = days + 1;
    (year as u32, month, day as u32)
}
