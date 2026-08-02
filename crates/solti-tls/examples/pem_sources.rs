//! # PEM sources and validated material
//!
//! `solti-tls` accepts PEM from memory or files.
//! Constructors only store those sources.
//! `load` reads, parses, and validates every configured value.
//!
//! This example shows:
//!
//! - an in-memory server identity;
//! - file-backed server and client material;
//! - validated PEM returned for a transport adapter;
//! - private-key redaction in `Debug`;
//! - a typed file error with the PEM role and path.
//!
//! The certificate is generated only to keep the example self-contained.
//! Production applications normally load certificates issued outside the process.
//!
//! Run with `cargo run -p solti-tls --example pem_sources`.

use std::{fs, io};

use solti_tls::{ClientTlsConfig, PemRole, ServerTlsConfig, TlsError, TlsIdentity, TrustRoots};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-tls: PEM source lifecycle

  file path ──► read during load ──┐
                                   ├──► PEM bytes
  memory bytes ────────────────────┘

  PemSource + PrivateKeySource ──► TlsIdentity ─┐
                                                ├──► ServerTlsConfig / ClientTlsConfig
  PemSource ─────────────────────► TrustRoots ──┘               │
                                                                ├──► into_rustls_config()
                                                                │         └──► rustls config
                                                                └──► load()
                                                                          └──► validated PEM

  This example follows the load() branch.
  Constructors keep sources.           load() reads and validates them.
  A private key is never printed.      File failures retain role and path.
"#;

fn development_identity() -> ExampleResult<(Vec<u8>, Vec<u8>)> {
    let bundle = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    Ok((
        bundle.cert.pem().into_bytes(),
        bundle.signing_key.serialize_pem().into_bytes(),
    ))
}

fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Convert application-owned TLS sources into validated transport input without exposing private keys."
    );

    let (certificate_pem, private_key_pem) = development_identity()?;

    println!("[1/4] Configure an identity from bytes already owned by the application.");
    let memory_identity =
        TlsIdentity::from_pem_bytes(certificate_pem.clone(), private_key_pem.clone());
    let identity_debug = format!("{memory_identity:?}");
    assert!(identity_debug.contains("redacted"));
    assert!(!identity_debug.contains("BEGIN PRIVATE KEY"));
    println!("      Debug confirms private-key redaction: {identity_debug}");

    println!("[2/4] Load and validate the configured server identity.");
    let loaded = ServerTlsConfig::new(memory_identity).load()?;
    println!(
        "      Output for an adapter: certificate={} bytes, private key={} bytes.",
        loaded.identity().certificate_chain_pem().len(),
        loaded.identity().expose_private_key_pem().len(),
    );
    assert!(format!("{loaded:?}").contains("redacted"));

    println!("[3/4] Configure the same inputs as file paths.");
    let directory = tempfile::tempdir()?;
    let certificate_path = directory.path().join("server.crt");
    let private_key_path = directory.path().join("server.key");
    let roots_path = directory.path().join("server-ca.crt");
    fs::write(&certificate_path, &certificate_pem)?;
    fs::write(&private_key_path, &private_key_pem)?;
    fs::write(&roots_path, &certificate_pem)?;

    let file_server = ServerTlsConfig::new(TlsIdentity::from_pem_files(
        &certificate_path,
        &private_key_path,
    ));
    println!("      Construction succeeds before any file is read.");
    let loaded_server = file_server.load()?;
    println!(
        "      Server load reads and validates {} certificate bytes.",
        loaded_server.identity().certificate_chain_pem().len(),
    );

    let loaded_client = ClientTlsConfig::new(TrustRoots::from_pem_file(&roots_path)).load()?;
    println!(
        "      Client load returns {} trust-root bytes; client identity present={}",
        loaded_client.server_roots_pem().len(),
        loaded_client.identity().is_some(),
    );

    println!("[4/4] Observe a typed error at the deferred read boundary.");
    let missing_path = directory.path().join("missing-ca.crt");
    let error = ClientTlsConfig::new(TrustRoots::from_pem_file(&missing_path))
        .load()
        .expect_err("a missing trust-root file must fail during load");
    match error {
        TlsError::ReadPem {
            role: PemRole::ServerTrustRoots,
            path,
            ..
        } if path == missing_path => {
            println!("      Role: server trust roots.");
            println!("      Path: {path:?}");
        }
        other => {
            return Err(io::Error::other(format!("unexpected TLS error: {other}")).into());
        }
    }

    println!("\nResult: memory and file sources reach the same validated PEM boundary.");

    Ok(())
}
