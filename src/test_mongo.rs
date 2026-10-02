//! Dedicated test Mongo. Existing development resources retain their own lifecycle.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::config::TestMongo;
const OWNER_LABEL: &str = "dev.grove.test-state";

pub struct Manager {
    state: PathBuf,
    docker: OsString,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedMongo {
    pub id: String,
    pub config: TestMongo,
    pub owner: String,
}

impl OwnedMongo {
    pub fn uri(&self) -> String {
        format!(
            "mongodb://127.0.0.1:{}/?directConnection=true&replicaSet=rs0",
            self.config.port
        )
    }
}

impl Manager {
    pub fn new(state: PathBuf, docker: OsString) -> Result<Self> {
        std::fs::create_dir_all(&state)?;
        Ok(Self {
            state: state.canonicalize()?,
            docker,
        })
    }

    fn output(&self, args: &[String]) -> Result<std::process::Output> {
        Command::new(&self.docker)
            .args(args)
            .output()
            .context("running Docker for test Mongo")
    }

    fn command(&self, args: &[String]) -> Result<String> {
        let out = self.output(args)?;
        if !out.status.success() {
            bail!(
                "test Mongo Docker command failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8(out.stdout)?.trim().to_string())
    }

    fn inspect(&self, name: &str) -> Result<Option<Value>> {
        let out = self.output(&["inspect".into(), name.into()])?;
        if !out.status.success() {
            let error = String::from_utf8_lossy(&out.stderr);
            let missing = error.to_ascii_lowercase();
            if missing.contains("no such object") || missing.contains("no such container") {
                return Ok(None);
            }
            bail!("test Mongo inspection failed: {error}");
        }
        let rows: Vec<Value> = serde_json::from_slice(&out.stdout)?;
        Ok(rows.into_iter().next())
    }

    fn verify(&self, config: &TestMongo, value: &Value) -> Result<OwnedMongo> {
        let owner = self.state.to_string_lossy();
        if value["Config"]["Labels"][OWNER_LABEL].as_str() != Some(owner.as_ref()) {
            bail!(
                "test Mongo ownership mismatch for {}; refusing to adopt it",
                config.name
            );
        }
        if value["Config"]["Image"].as_str() != Some(&config.image) {
            bail!("test Mongo image does not match {}", config.image);
        }
        let bindings = &value["HostConfig"]["PortBindings"]["27017/tcp"];
        if bindings
            != &serde_json::json!([{"HostIp": "127.0.0.1", "HostPort": config.port.to_string()}])
        {
            bail!("test Mongo requires exactly its configured loopback port mapping");
        }
        if value["Config"]["Cmd"] != serde_json::json!(["--replSet", "rs0", "--bind_ip_all"]) {
            bail!("test Mongo replica-set command does not match");
        }
        let id = value["Id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("test Mongo has no container ID")?;
        Ok(OwnedMongo {
            id: id.to_string(),
            config: config.clone(),
            owner: owner.into_owned(),
        })
    }

    pub fn ensure(&self, config: &TestMongo) -> Result<OwnedMongo> {
        let expected = config.version()?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.state.join("resource.lock"))?;
        lock.lock()?;
        let value = if let Some(value) = self.inspect(&config.name)? {
            value
        } else {
            let listener = std::net::TcpListener::bind(("127.0.0.1", config.port))
                .context("test Mongo port is occupied by an unowned listener")?;
            drop(listener);
            self.command(&[
                "run".into(),
                "-d".into(),
                "--name".into(),
                config.name.clone(),
                "--label".into(),
                format!("{OWNER_LABEL}={}", self.state.display()),
                "--ulimit".into(),
                "nofile=64000:64000".into(),
                "-p".into(),
                format!("127.0.0.1:{}:27017", config.port),
                config.image.clone(),
                "--replSet".into(),
                "rs0".into(),
                "--bind_ip_all".into(),
            ])?;
            self.inspect(&config.name)?
                .context("test Mongo disappeared after creation")?
        };
        let owned = self.verify(config, &value)?;
        if value["State"]["Running"] != true {
            self.command(&["start".into(), owned.id.clone()])?;
        }
        let script = format!(
            r#"
if (db.version() !== {expected:?}) throw new Error('test Mongo version mismatch: ' + db.version());
try {{ rs.status(); }} catch (e) {{
    if (e.code !== 94) throw e;
    const result = rs.initiate({{_id:'rs0',members:[{{_id:0,host:'127.0.0.1:27017'}}]}});
    if (result.ok !== 1 && result.code !== 23) throw new Error(JSON.stringify(result));
}}
const h = db.hello();
print(JSON.stringify({{version:db.version(),set_name:h.setName || null,primary:h.isWritablePrimary === true}}));
"#
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let output = match self.eval(&owned, &script) {
                Ok(output) => output,
                Err(error)
                    if error
                        .to_string()
                        .contains("MongoNetworkError: connect ECONNREFUSED") =>
                {
                    if Instant::now() >= deadline {
                        return Err(error)
                            .context("test Mongo did not accept connections within 30s");
                    }
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let status: Value =
                serde_json::from_str(&output).context("reading test Mongo readiness")?;
            if status["version"].as_str() != Some(expected) {
                bail!(
                    "test Mongo version differs from {expected}: {}",
                    status["version"]
                );
            }
            if let Some(name) = status["set_name"].as_str()
                && name != "rs0"
            {
                bail!("test Mongo replica-set name differs from rs0");
            }
            if status["primary"] == true && status["set_name"] == "rs0" {
                return Ok(owned);
            }
            if Instant::now() >= deadline {
                bail!("test Mongo did not become a writable primary within 30s");
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    pub fn drop_databases(&self, owned: &OwnedMongo, databases: &[String]) -> Result<()> {
        if databases.iter().any(|name| {
            !name.starts_with("grove_test_")
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }) {
            bail!("refusing cleanup of a database outside the test namespace");
        }
        let current = self
            .inspect(&owned.config.name)?
            .context("test Mongo container is absent; cleanup retained")?;
        let verified = self.verify(&owned.config, &current)?;
        if verified.id != owned.id || verified.owner != owned.owner {
            bail!("test Mongo identity changed; cleanup retained");
        }
        let names = serde_json::to_string(databases)?;
        self.eval(owned, &format!("for (const name of {names}) {{ const r = db.getSiblingDB(name).dropDatabase(); if (r.ok !== 1) throw new Error(JSON.stringify(r)); }}"))?;
        Ok(())
    }

    fn eval(&self, owned: &OwnedMongo, script: &str) -> Result<String> {
        self.command(&["exec".into(), owned.id.clone(), "mongosh".into(), "--quiet".into(),
            "mongodb://127.0.0.1:27017/?directConnection=true&serverSelectionTimeoutMS=2000&connectTimeoutMS=2000".into(),
            "--eval".into(), script.into()])
    }
}
