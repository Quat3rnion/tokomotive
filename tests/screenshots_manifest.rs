use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Manifest {
    captures: Vec<Capture>,
    exclusions: Vec<Exclusion>,
    surface_globs: Vec<String>,
}

#[derive(Deserialize)]
struct Capture {
    image: String,
    page: String,
    route: String,
    recipe: String,
    caption: String,
    alt: String,
    kind: String,
    #[serde(default)]
    theme: Option<String>,
}

#[derive(Deserialize)]
struct Exclusion {
    route: String,
    reason: String,
}

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files
}

// Read literal .route("/admin/...", get(...)) registrations, including those
// split over lines and in merged routers. No hand-maintained page list.
fn admin_get_routes(root: &Path) -> BTreeSet<String> {
    let mut routes = BTreeSet::new();
    for file in rust_files(&root.join("src/web")) {
        let source = fs::read_to_string(file).unwrap();
        for registration in source.split(".route(").skip(1) {
            let Some(rest) = registration.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some((route, handler)) = rest.split_once('"') else {
                continue;
            };
            let handler = handler.trim_start().strip_prefix(',').unwrap().trim_start();
            if (route == "/admin" || route.starts_with("/admin/")) && handler.starts_with("get(") {
                routes.insert(route.to_owned());
            }
        }
    }
    assert!(
        routes.contains("/admin"),
        "route discovery found no dashboard"
    );
    routes
}

#[test]
fn screenshots_cover_dashboard_routes_and_readme() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest: Manifest = serde_json::from_str(
        &fs::read_to_string(root.join("scripts/screenshots/manifest.json")).unwrap(),
    )
    .unwrap();
    let registered = admin_get_routes(root);
    let readme = fs::read_to_string(root.join("README.md")).unwrap();
    let mut covered = BTreeSet::new();
    let mut images = BTreeSet::new();
    assert!(!manifest.surface_globs.is_empty());
    for capture in manifest.captures {
        assert!(
            registered.contains(&capture.route),
            "unknown route {}",
            capture.route
        );
        assert!(capture.page.starts_with("/admin"));
        assert!(
            !capture.recipe.is_empty() && !capture.caption.is_empty() && !capture.alt.is_empty()
        );
        assert!(matches!(
            capture.kind.as_str(),
            "hero" | "hero-light" | "table"
        ));
        // The default capture theme is dark; a light hero is the
        // prefers-color-scheme source of a <picture>.
        assert_eq!(
            capture.theme.as_deref() == Some("light"),
            capture.kind == "hero-light",
            "theme of {}",
            capture.image
        );
        assert!(matches!(capture.theme.as_deref(), None | Some("light")));
        let reference = if capture.kind == "hero" {
            format!(
                "<img alt=\"{}\" src=\"docs/images/{}\">",
                capture.alt.replace('"', "&quot;"),
                capture.image
            )
        } else if capture.kind == "hero-light" {
            format!(
                "<source media=\"(prefers-color-scheme: light)\" srcset=\"docs/images/{}\">",
                capture.image
            )
        } else {
            format!(
                "[{}](docs/images/{}) | {}",
                capture.caption, capture.image, capture.alt
            )
        };
        assert!(
            readme.contains(&reference),
            "README text differs for {}",
            capture.image
        );
        assert!(!capture.image.contains('/') && capture.image.ends_with(".png"));
        assert!(images.insert(capture.image.clone()), "duplicate image");
        assert!(
            root.join("docs/images").join(&capture.image).is_file(),
            "missing image {}",
            capture.image
        );
        covered.insert(capture.route);
    }
    for exclusion in manifest.exclusions {
        assert!(
            registered.contains(&exclusion.route),
            "unknown exclusion {}",
            exclusion.route
        );
        assert!(!exclusion.reason.trim().is_empty());
        assert!(
            covered.insert(exclusion.route),
            "route both captured and excluded"
        );
    }
    assert_eq!(
        registered, covered,
        "each admin GET needs a capture or reason"
    );

    let referenced: BTreeSet<_> = readme
        .split("docs/images/")
        .skip(1)
        .filter_map(|suffix| {
            let name = suffix.split([')', '"', '\'', ' ', '\n']).next().unwrap();
            name.ends_with(".png").then(|| name.to_owned())
        })
        .collect();
    assert_eq!(images, referenced, "README and screenshot manifest differ");
}

#[test]
fn screenshot_push_hook_contract() {
    let status = std::process::Command::new("bash")
        .arg("scripts/screenshots/test-hook.sh")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .unwrap();
    assert!(status.success(), "screenshot pre-push regression failed");
}
