//! The model server (src/embed/serve.rs): a search's query embedded in a
//! `lore model serve` process gets the vector a model of its own would give,
//! a wrong token is refused, and the server ends once idle. Needs the nomic
//! model installed; without it the test says so and passes.
use codelore::embed::{self, EmbeddingModel, Embedder, Embeddings, ModelSource};
use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

#[test]
fn a_query_embedded_by_the_server_is_the_local_vector() {
    let model = EmbeddingModel::Nomic;
    if !embed::status_for(model, &embed::model_dir(model)).available {
        eprintln!("skipped: {} is not installed", model.id());
        return;
    }
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_lore"))
        .args(["model", "serve", "--model", model.id()])
        .env("LORE_EMBED_SERVE_IDLE", "3")
        // started from another binary than this test's: it says this one's build
        .env("LORE_SERVE_BUILD", embed::serve::build())
        .stdin(std::process::Stdio::null())
        .spawn()
        .expect("lore model serve starts");
    let state = embed::serve::state_file(model, &embed::serve::build());
    let started = Instant::now();
    let remote = loop {
        let ours = std::fs::read_to_string(&state).is_ok_and(|text| text.contains(&format!("\"pid\":{}", child.id())));
        if ours {
            if let Some(remote) = embed::serve::connect(model) {
                break remote;
            }
        }
        assert!(started.elapsed() < Duration::from_secs(60), "the server never answered");
        std::thread::sleep(Duration::from_millis(100));
    };
    // Read now: the server ends after three idle seconds, and opening the
    // local model below can take longer than that on a busy machine.
    let port: u16 = serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&state).unwrap()).unwrap()["port"].as_u64().unwrap() as u16;
    // a request without the token gets nothing
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.write_all(b"{\"token\":\"guess\",\"texts\":[\"x\"]}\n").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert!(line.contains("wrong token") && !line.contains("vectors"), "{line}");

    let text = format!("{}why does the probe view lose its direction", model.query_prefix());
    let served = remote.embed(&[text.clone()]).expect("the server embeds");
    let local = Embedder::open_model_for_queries(model, ModelSource::Cache).unwrap().embed_prefixed(&[text.as_str()]).unwrap();
    let diff = served[0].iter().zip(&local[0]).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(diff < 1e-5, "server and local vectors differ by {diff}");

    // idle for 3 s: it ends and takes its state file with it
    let started = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(30), "the idle server did not end");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(!std::fs::read_to_string(&state).is_ok_and(|text| text.contains(&format!("\"pid\":{}", child.id()))));
}
