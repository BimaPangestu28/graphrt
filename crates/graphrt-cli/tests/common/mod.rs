//! Helpers shared by integration tests: a fake model endpoint and a local,
//! signed skill catalog.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

/// An OpenAI compatible endpoint on a local port. It answers each chat
/// request with the next text of `answers` (then with `ok`), and keeps what
/// it was sent.
#[derive(Clone, Debug)]
pub struct FakeModel {
    /// The value for `OPENAI_BASE_URL`.
    pub url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl FakeModel {
    pub fn start(answers: &[&str]) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let mut answers: VecDeque<String> = answers.iter().map(|answer| (*answer).to_owned()).collect();
        // The thread ends with the test process.
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { continue };
                let request = read_request(&mut socket);
                seen.lock().unwrap().push(request);
                let answer = answers.pop_front().unwrap_or_else(|| "ok".to_owned());
                let chunk = serde_json::json!({"choices": [{"index": 0, "delta": {"content": answer}}]});
                let done = serde_json::json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
                let body = format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n");
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).and_then(|()| socket.write_all(body.as_bytes()));
            }
        });
        Self { url, requests }
    }

    /// The requests received so far, oldest first, as raw HTTP text.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// The `(name, value)` pairs that point the `openai` provider at this endpoint.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![("OPENAI_API_KEY".to_owned(), "test-key".to_owned()), ("OPENAI_BASE_URL".to_owned(), self.url.clone())]
    }
}

/// Read one HTTP request with its body.
fn read_request(socket: &mut std::net::TcpStream) -> String {
    let mut received = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = socket.read(&mut buffer).unwrap_or(0);
        if read == 0 {
            break;
        }
        received.extend_from_slice(&buffer[..read]);
        let seen = String::from_utf8_lossy(&received);
        let Some(head_end) = seen.find("\r\n\r\n") else { continue };
        let length = seen
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0))
            })
            .unwrap_or(0usize);
        if received.len() >= head_end + 4 + length {
            break;
        }
    }
    String::from_utf8_lossy(&received).into_owned()
}

/// Plugin options that load nothing from the machine the tests run on.
#[cfg(feature = "serve")]
pub fn no_plugins() -> graphrt_cli::extensions::PluginOptions {
    graphrt_cli::extensions::PluginOptions {
        dirs: Vec::new(),
        no_default: true,
        trusted: Vec::new(),
        pool: Default::default(),
        allow_private_network: false,
    }
}

#[cfg(all(feature = "skills", feature = "plugins"))]
pub mod catalog {
    //! A skill catalog in a scratch directory: `index-v2.json` and archives
    //! behind `file://` URLs, signed with a throwaway minisign key.

    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    use sha2::{Digest, Sha256};

    pub const SKILL: &str = "---\nname: kebijakan-refund\ndescription: Menilai permintaan refund. Gunakan saat pelanggan meminta uang kembali.\nmetadata:\n  version: \"1.0.0\"\n  display_name: Kebijakan Refund dan Retur\nallowed-tools: shopify/get_order\n---\n# Menilai permintaan refund\n\n1. Kumpulkan nomor pesanan.\n";

    pub struct Catalog {
        pub root: PathBuf,
    }

    impl Drop for Catalog {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The key the tests name as the trusted catalog key.
    pub fn key_pair() -> &'static minisign::KeyPair {
        static KEY_PAIR: OnceLock<minisign::KeyPair> = OnceLock::new();
        KEY_PAIR.get_or_init(|| minisign::KeyPair::generate_unencrypted_keypair().expect("test key pair"))
    }

    pub fn sign_with(key: &minisign::KeyPair, file: &Path) {
        let bytes = std::fs::read(file).unwrap();
        let comment = "graphrt-plugins index-v2.json plugins=0 generated_at=test";
        let signature =
            minisign::sign(Some(&key.pk), &key.sk, std::io::Cursor::new(bytes), Some(comment), None).unwrap();
        std::fs::write(format!("{}.minisig", file.display()), signature.into_string()).unwrap();
    }

    pub fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// A `.tar.gz` of regular files (`type_flag` `0`) and, for one test, a link (`2`).
    pub fn tar_gz(entries: &[(&str, u8, &[u8], &str)]) -> Vec<u8> {
        let mut tar = Vec::new();
        for (path, type_flag, content, link) in entries {
            let mut header = [0u8; 512];
            header[..path.len()].copy_from_slice(path.as_bytes());
            let octal = |field: &mut [u8], value: u64| {
                let text = format!("{value:0width$o}\0", width = field.len() - 1);
                field.copy_from_slice(text.as_bytes());
            };
            octal(&mut header[100..108], 0o644);
            octal(&mut header[108..116], 0);
            octal(&mut header[116..124], 0);
            octal(&mut header[124..136], content.len() as u64);
            octal(&mut header[136..148], 0);
            header[156] = *type_flag;
            header[157..157 + link.len()].copy_from_slice(link.as_bytes());
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            header[148..156].copy_from_slice(b"        ");
            let checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
            header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
            tar.extend_from_slice(&header);
            tar.extend_from_slice(content);
            tar.resize(tar.len().div_ceil(512) * 512, 0);
        }
        tar.resize(tar.len() + 1024, 0);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    pub fn skill_archive(skill_md: &str) -> Vec<u8> {
        tar_gz(&[
            ("kebijakan-refund/SKILL.md", b'0', skill_md.as_bytes(), ""),
            ("kebijakan-refund/references/matriks.md", b'0', b"| hari | keputusan |\n", ""),
        ])
    }

    impl Catalog {
        pub fn new() -> Self {
            Self { root: graphrt_core::private_fs::create_fresh_temp_dir("graphrt-skill-catalog").unwrap() }
        }

        pub fn file_url(&self, name: &str) -> String {
            let path = self.root.join("catalog").join(name);
            let text = path.to_string_lossy().replace('\\', "/");
            if text.starts_with('/') { format!("file://{text}") } else { format!("file:///{text}") }
        }

        /// Write an archive and return its catalog entry.
        pub fn publish(&self, version: &str, archive: &[u8]) -> serde_json::Value {
            let name = format!("skill-kebijakan-refund-{version}.tar.gz");
            std::fs::create_dir_all(self.root.join("catalog")).unwrap();
            std::fs::write(self.root.join("catalog").join(&name), archive).unwrap();
            serde_json::json!({
                "name": "kebijakan-refund", "display_name": "Kebijakan Refund dan Retur", "version": version,
                "description": "Menilai permintaan refund.", "tarball_url": self.file_url(&name),
                "sha256": sha256_hex(archive), "size_bytes": archive.len(), "publisher": "graphrt",
            })
        }

        /// Write `index-v2.json` with `skills` (unsigned) and return its path.
        pub fn write_index(&self, skills: &[serde_json::Value]) -> PathBuf {
            let index = serde_json::json!({
                "schema": 2, "generated_at": "2026-11-27T10:00:00Z", "plugins": [], "skills": skills,
            });
            let path = self.root.join("catalog/index-v2.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, serde_json::to_vec(&index).unwrap()).unwrap();
            path
        }

        pub fn signed_index(&self, skills: &[serde_json::Value]) -> PathBuf {
            let path = self.write_index(skills);
            sign_with(key_pair(), &path);
            path
        }
    }
}
