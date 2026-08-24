use super::*;
use helix_term::application::Application;

fn rs_app(content: &str) -> anyhow::Result<Application> {
    let file = tempfile::Builder::new()
        .suffix(".rs")
        .tempfile()?;
    std::fs::write(file.path(), content)?;
    AppBuilder::new()
        .with_file(file.path(), None)
        .with_lang_loader(test_syntax_loader(None))
        .build()
}

fn current_text(app: &Application) -> String {
    let (_view, doc) = helix_view::current_ref!(app.editor);
    doc.text().slice(..).to_string()
}

/// Smoke A: `t ?` opens the prefix-aware Explorer through REAL key
/// dispatch; accepting the highlighted entry feeds the pending-target
/// pipeline (`target: …` status), and a subsequent action key executes.
#[tokio::test(flavor = "multi_thread")]
async fn tachyon_explorer_opens_and_feeds_pending_target() -> anyhow::Result<()> {
    let mut app = rs_app("alpha beta gamma\n")?;

    test_key_sequences(
        &mut app,
        vec![
            (
                Some("t?<ret>"),
                Some(&|app: &Application| {
                    // First Explorer row is the `word` target; accepting it
                    // must propagate into the pending-target state.
                    let status = app
                        .editor
                        .get_status()
                        .map(|(s, _)| s.to_string())
                        .unwrap_or_default();
                    assert!(
                        status.contains("target:"),
                        "expected pending-target status after explorer pick, got {status:?}"
                    );
                }),
            ),
            (
                Some("d"),
                Some(&|app: &Application| {
                    let text = current_text(app);
                    assert_eq!(
                        text.trim_start_matches(' ').trim(),
                        "beta gamma",
                        "word deleted via explorer-selected target"
                    );
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

/// Smoke B: `t -n d .` — backward counted parameter deletion followed by
/// semantic repeat against the CURRENT tree, through real key dispatch.
#[tokio::test(flavor = "multi_thread")]
async fn tachyon_backward_parameter_delete_with_repeat() -> anyhow::Result<()> {
    let mut app = rs_app("fn f(a: i32, b: i32, c: i32) {}\n")?;

    test_key_sequences(
        &mut app,
        vec![(
            // Jump onto `c`, delete the previous parameter, repeat.
            Some("/c<ret>t-nd."),
            Some(&|app: &Application| {
                let text = current_text(app);
                assert_eq!(
                    text, "fn f(c: i32) {}\n",
                    "two backward delimiter-aware deletions with separator repair"
                );
            }),
        )],
        false,
    )
    .await?;
    Ok(())
}

/// Smoke C: `t 2 f c` — counted Change through Helix's native multi-cursor
/// insert; typed replacement appears at BOTH changed functions.
#[tokio::test(flavor = "multi_thread")]
async fn tachyon_counted_change_multi_cursor_insert() -> anyhow::Result<()> {
    let src = "fn alpha() {\n    let a = 1;\n}\n\nfn beta() {\n    let b = 2;\n}\n\nfn gamma() {\n    let g = 3;\n}\n";
    let mut app = rs_app(src)?;

    test_key_sequences(
        &mut app,
        vec![(
            Some("/let a<ret>t2fcX<esc>"),
            Some(&|app: &Application| {
                let text = current_text(app);
                assert_eq!(
                    text,
                    "fn alpha() X\n\nfn beta() X\n\nfn gamma() {\n    let g = 3;\n}\n",
                    "counted Change clears both bodies and inserts once per cursor"
                );
            }),
        )],
        false,
    )
    .await?;
    Ok(())
}
