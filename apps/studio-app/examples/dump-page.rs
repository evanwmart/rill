fn main() {
    let s = studio_app::Studio::new(std::path::PathBuf::from("/nonexistent/theme.toml"));
    use rill_server::AppHandler;
    let section = std::env::var("STUDIO_SECTION").unwrap_or_else(|_| "density".into());
    let page = s
        .get(&format!("/studio/{section}"), &rill_auth::Identity::Anonymous)
        .expect("page");
    std::fs::write(std::env::args().nth(1).unwrap(), page).unwrap();
}
