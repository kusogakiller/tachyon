//! Tachyon Speed Benchmark
//!
//! Measures task completion time for realistic editing operations.
//! Compares Tachyon, Helix, and Vim key-count metrics.
//!
//! Run with: cargo bench -p helix-term --features integration --bench tachyon_speed

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use helix_core::Transaction;
use helix_loader::workspace_trust::WorkspaceTrust;
use helix_term::application::Application;
use helix_term::args::Args;
use helix_view::input::parse_macro;

#[cfg(windows)]
use crossterm::event::{Event, KeyEvent};
#[cfg(not(windows))]
use termina::event::{Event, KeyEvent};

// ---------------------------------------------------------------------------
// Static analysis: key counts, mode transitions, cognitive complexity
// ---------------------------------------------------------------------------

struct CommandMapping {
    task: &'static str,
    tachyon_keys: &'static str,
    tachyon_key_count: usize,
    tachyon_mode_transitions: usize,
    vim_keys: &'static str,
    vim_key_count: usize,
    vim_mode_transitions: usize,
    helix_keys: &'static str,
    helix_key_count: usize,
    helix_mode_transitions: usize,
    tachyon_description: &'static str,
    vim_description: &'static str,
    helix_description: &'static str,
}

fn task_matrix() -> Vec<CommandMapping> {
    vec![
        CommandMapping {
            task: "change word",
            tachyon_keys: "twc",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: "ciw",
            vim_key_count: 3,
            vim_mode_transitions: 2,
            helix_keys: "xc",
            helix_key_count: 2,
            helix_mode_transitions: 2,
            tachyon_description: "target word, change",
            vim_description: "change inside word",
            helix_description: "extend to word, change",
        },
        CommandMapping {
            task: "delete word",
            tachyon_keys: "twd",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: "diw",
            vim_key_count: 3,
            vim_mode_transitions: 1,
            helix_keys: "xd",
            helix_key_count: 2,
            helix_mode_transitions: 1,
            tachyon_description: "target word, delete",
            vim_description: "delete inside word",
            helix_description: "extend to word, delete",
        },
        CommandMapping {
            task: "yank word",
            tachyon_keys: "twy",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: "yiw",
            vim_key_count: 3,
            vim_mode_transitions: 1,
            helix_keys: "xy",
            helix_key_count: 2,
            helix_mode_transitions: 1,
            tachyon_description: "target word, yank",
            vim_description: "yank inside word",
            helix_description: "extend to word, yank",
        },
        CommandMapping {
            task: "change function",
            tachyon_keys: "tfc",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: "vafd",  // visual select around function, delete (closest equivalent)
            vim_key_count: 4,
            vim_mode_transitions: 2,
            helix_keys: "mafc", // match around function, change
            helix_key_count: 4,
            helix_mode_transitions: 2,
            tachyon_description: "target function, change",
            vim_description: "visual select around function (textobj required)",
            helix_description: "select around function, change",
        },
        CommandMapping {
            task: "delete function",
            tachyon_keys: "tfd",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: "vafd",
            vim_key_count: 4,
            vim_mode_transitions: 2,
            helix_keys: "mafd",
            helix_key_count: 4,
            helix_mode_transitions: 2,
            tachyon_description: "target function, delete",
            vim_description: "visual select around function, delete",
            helix_description: "select around function, delete",
        },
        CommandMapping {
            task: "delete statement",
            tachyon_keys: "tsd",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: "das",
            vim_key_count: 3,
            vim_mode_transitions: 1,
            helix_keys: "x",  // no direct statement object in stock helix
            helix_key_count: 1,
            helix_mode_transitions: 1,
            tachyon_description: "target statement, delete",
            vim_description: "delete around statement (treesitter textobj)",
            helix_description: "no direct equivalent",
        },
        CommandMapping {
            task: "change expression",
            tachyon_keys: "tec",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: "die",
            vim_key_count: 3,
            vim_mode_transitions: 1,
            helix_keys: "x",
            helix_key_count: 1,
            helix_mode_transitions: 1,
            tachyon_description: "target expression, change",
            vim_description: "no direct equivalent in vanilla vim",
            helix_description: "no direct equivalent",
        },
        CommandMapping {
            task: "indent line",
            tachyon_keys: "tl>",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: ">>",
            vim_key_count: 2,
            vim_mode_transitions: 0,
            helix_keys: ">",
            helix_key_count: 1,
            helix_mode_transitions: 0,
            tachyon_description: "target line, indent",
            vim_description: "indent line",
            helix_description: "indent selection",
        },
        CommandMapping {
            task: "change string",
            tachyon_keys: "t\"c",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: "ci\"",
            vim_key_count: 3,
            vim_mode_transitions: 2,
            helix_keys: "mi\"c",
            helix_key_count: 4,
            helix_mode_transitions: 2,
            tachyon_description: "target string, change",
            vim_description: "change inside quotes",
            helix_description: "select inside quotes, change",
        },
        CommandMapping {
            task: "change all occurrences",
            tachyon_keys: "tw <A-a> c",
            tachyon_key_count: 4,
            tachyon_mode_transitions: 0,
            vim_keys: "*cgn",
            vim_key_count: 4,
            vim_mode_transitions: 2,
            helix_keys: "*",
            helix_key_count: 1,
            helix_mode_transitions: 1,
            tachyon_description: "target word, select all, change all",
            vim_description: "search word, change next match (repeat with .)",
            helix_description: "search word, then manually navigate",
        },
        CommandMapping {
            task: "change paragraph",
            tachyon_keys: "tpc",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: "dap",
            vim_key_count: 3,
            vim_mode_transitions: 1,
            helix_keys: "x",
            helix_key_count: 1,
            helix_mode_transitions: 1,
            tachyon_description: "target paragraph, change",
            vim_description: "delete around paragraph",
            helix_description: "extend selection (no paragraph object)",
        },
        CommandMapping {
            task: "change block",
            tachyon_keys: "tbc",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: "vi{\"gc",
            vim_key_count: 5,
            vim_mode_transitions: 3,
            helix_keys: "mibc",
            helix_key_count: 4,
            helix_mode_transitions: 2,
            tachyon_description: "target block, change",
            vim_description: "no direct equivalent",
            helix_description: "select inside block, change",
        },
        CommandMapping {
            task: "change class",
            tachyon_keys: "tgc",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: "vacd",
            vim_key_count: 4,
            vim_mode_transitions: 2,
            helix_keys: "mic",
            helix_key_count: 3,
            helix_mode_transitions: 1,
            tachyon_description: "target class, change",
            vim_description: "visual around class (textobj required)",
            helix_description: "select inside class (language-dependent)",
        },
        CommandMapping {
            task: "toggle case word",
            tachyon_keys: "tw~",
            tachyon_key_count: 3,
            tachyon_mode_transitions: 0,
            vim_keys: "gUiw",
            vim_key_count: 4,
            vim_mode_transitions: 1,
            helix_keys: "~",
            helix_key_count: 1,
            helix_mode_transitions: 0,
            tachyon_description: "target word, toggle case",
            vim_description: "uppercase word",
            helix_description: "toggle case (single char)",
        },
    ]
}

fn print_analysis() {
    let tasks = task_matrix();

    println!("\n{:=<100}", "");
    println!("TACHYON SPEED BENCHMARK — STATIC ANALYSIS");
    println!("{:=<100}\n", "");

    println!("{:<25} {:>8} {:>8} {:>8} │ {:>8} {:>8} {:>8} │ {:>8} {:>8} {:>8}",
        "Task", "T-Keys", "T-Trans", "T-Total",
        "V-Keys", "V-Trans", "V-Total",
        "H-Keys", "H-Trans", "H-Total");
    println!("{:-<100}", "");

    let mut tachyon_total = 0;
    let mut vim_total = 0;
    let mut helix_total = 0;
    let mut tachyon_trans_total = 0;
    let mut vim_trans_total = 0;
    let mut helix_trans_total = 0;

    for t in &tasks {
        let t_total = t.tachyon_key_count + t.tachyon_mode_transitions;
        let v_total = t.vim_key_count + t.vim_mode_transitions;
        let h_total = t.helix_key_count + t.helix_mode_transitions;
        println!("{:<25} {:>8} {:>8} {:>8} │ {:>8} {:>8} {:>8} │ {:>8} {:>8} {:>8}",
            t.task,
            t.tachyon_key_count, t.tachyon_mode_transitions, t_total,
            t.vim_key_count, t.vim_mode_transitions, v_total,
            t.helix_key_count, t.helix_mode_transitions, h_total);
        tachyon_total += t.tachyon_key_count;
        vim_total += t.vim_key_count;
        helix_total += t.helix_key_count;
        tachyon_trans_total += t.tachyon_mode_transitions;
        vim_trans_total += t.vim_mode_transitions;
        helix_trans_total += t.helix_mode_transitions;
    }

    println!("{:-<100}", "");
    println!("{:<25} {:>8} {:>8} {:>8} │ {:>8} {:>8} {:>8} │ {:>8} {:>8} {:>8}",
        "TOTAL",
        tachyon_total, tachyon_trans_total, tachyon_total + tachyon_trans_total,
        vim_total, vim_trans_total, vim_total + vim_trans_total,
        helix_total, helix_trans_total, helix_total + helix_trans_total);

    let n = tasks.len() as f64;
    println!("\nAverages per task:");
    println!("  Tachyon: {:.1} keys, {:.1} mode transitions, {:.1} total",
        tachyon_total as f64 / n, tachyon_trans_total as f64 / n,
        (tachyon_total + tachyon_trans_total) as f64 / n);
    println!("  Vim:     {:.1} keys, {:.1} mode transitions, {:.1} total",
        vim_total as f64 / n, vim_trans_total as f64 / n,
        (vim_total + vim_trans_total) as f64 / n);
    println!("  Helix:   {:.1} keys, {:.1} mode transitions, {:.1} total",
        helix_total as f64 / n, helix_trans_total as f64 / n,
        (helix_total + helix_trans_total) as f64 / n);

    // 10 KPS timing model
    println!("\n{:=<100}", "");
    println!("10 KPS HUMAN INPUT MODEL (1 key ≈ 100ms)");
    println!("{:=<100}\n", "");

    println!("{:<25} {:>14} {:>14} {:>14} {:>10}",
        "Task", "Tachyon", "Vim", "Helix", "T faster?");
    println!("{:-<80}", "");

    for t in &tasks {
        let t_time = t.tachyon_key_count as f64 * 100.0;
        let v_time = t.vim_key_count as f64 * 100.0;
        let h_time = t.helix_key_count as f64 * 100.0;
        let faster = if t.vim_key_count > t.tachyon_key_count {
            format!("-{}ms", (t.vim_key_count - t.tachyon_key_count) * 100)
        } else if t.vim_key_count < t.tachyon_key_count {
            format!("+{}ms", (t.tachyon_key_count - t.vim_key_count) * 100)
        } else {
            "=".to_string()
        };
        println!("{:<25} {:>12.0}ms {:>12.0}ms {:>12.0}ms {:>10}",
            t.task, t_time, v_time, h_time, faster);
    }

    // Detailed command mapping
    println!("\n{:=<100}", "");
    println!("DETAILED COMMAND MAPPING");
    println!("{:=<100}\n", "");

    for t in &tasks {
        println!("Task: {}", t.task);
        println!("  Tachyon: {:<12} ({}) [{} transitions]",
            t.tachyon_keys, t.tachyon_description, t.tachyon_mode_transitions);
        println!("  Vim:     {:<12} ({}) [{} transitions]",
            t.vim_keys, t.vim_description, t.vim_mode_transitions);
        println!("  Helix:   {:<12} ({}) [{} transitions]",
            t.helix_keys, t.helix_description, t.helix_mode_transitions);
        println!();
    }
}

// ---------------------------------------------------------------------------
// Dynamic benchmark: actual execution time measurement
// ---------------------------------------------------------------------------

fn make_app() -> anyhow::Result<Application> {
    helix_loader::initialize_config_file(None);
    helix_loader::initialize_log_file(None);

    let config = helix_term::config::Config::load_default().unwrap_or_default();
    let lang_config = helix_loader::config::default_lang_config();
    let syn_loader = helix_core::syntax::Loader::new(lang_config.try_into().unwrap()).unwrap();

    Application::new(
        Args::default(),
        config,
        syn_loader,
        WorkspaceTrust::fully_trusted(),
    )
}

fn set_input_text(app: &mut Application, text: &str) {
    let (view, doc) = helix_view::current!(app.editor);
    let sel = doc.selection(view.id).clone();
    let text_tendril: helix_core::Tendril = text.into();
    let transaction = Transaction::change_by_selection(doc.text(), &sel, |_| {
        (0, doc.text().len_chars(), Some(text_tendril.clone()))
    });
    doc.apply(&transaction, view.id);
}

fn bench_dynamic(c: &mut Criterion) {
    let mut group = c.benchmark_group("tachyon_execution");
    group.sample_size(10);

    let test_cases: Vec<(&str, &str, &str)> = vec![
        ("change word", "hello world", "twc"),
        ("delete word", "hello world", "twd"),
        ("yank word", "hello world", "twy"),
        ("change string", "say hello", "twc"),
        ("delete function", "fn foo() { let x = 1; }", "tfd"),
    ];

    for (name, input, keys) in &test_cases {
        group.bench_function(BenchmarkId::new("tachyon", name), |b| {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            b.iter(|| {
                runtime.block_on(async {
                    let mut app = make_app().unwrap();
                    set_input_text(&mut app, input);

                    let key_events = parse_macro(keys).unwrap();
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    let mut rx_stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);

                    for key_event in key_events {
                        let crossterm_key = KeyEvent::from(key_event);
                        tx.send(Ok(Event::Key(crossterm_key))).unwrap();
                    }
                    drop(tx);

                    app.event_loop_until_idle(&mut rx_stream).await;

                    std::hint::black_box(&app);
                });
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn full_benchmark(c: &mut Criterion) {
    print_analysis();
    bench_dynamic(c);
}

criterion_group!(benches, full_benchmark);
criterion_main!(benches);
