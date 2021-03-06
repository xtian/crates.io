#![allow(missing_debug_implementations)]

use diesel::prelude::*;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use tempdir::TempDir;
use url::Url;

use crate::background::Job;
use crate::background_jobs::Environment;
use crate::models::{DependencyKind, Version};
use crate::schema::versions;
use crate::util::{internal, CargoResult};

#[derive(Serialize, Deserialize, Debug)]
pub struct Crate {
    pub name: String,
    pub vers: String,
    pub deps: Vec<Dependency>,
    pub cksum: String,
    pub features: HashMap<String, Vec<String>>,
    pub yanked: Option<bool>,
    #[serde(default)]
    pub links: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Dependency {
    pub name: String,
    pub req: String,
    pub features: Vec<String>,
    pub optional: bool,
    pub default_features: bool,
    pub target: Option<String>,
    pub kind: Option<DependencyKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
}

pub struct Repository {
    checkout_path: TempDir,
    repository: git2::Repository,
}

impl Repository {
    pub fn open(url: &Url) -> CargoResult<Self> {
        let checkout_path = TempDir::new("git")?;
        let repository = git2::Repository::clone(url.as_str(), checkout_path.path())?;

        // All commits to the index registry made through crates.io will be made by bors, the Rust
        // community's friendly GitHub bot.
        let mut cfg = repository.config()?;
        cfg.set_str("user.name", "bors")?;
        cfg.set_str("user.email", "bors@rust-lang.org")?;

        Ok(Self {
            checkout_path,
            repository,
        })
    }

    fn index_file(&self, name: &str) -> PathBuf {
        self.checkout_path
            .path()
            .join(self.relative_index_file(name))
    }

    fn relative_index_file(&self, name: &str) -> PathBuf {
        let name = name.to_lowercase();
        match name.len() {
            1 => Path::new("1").join(&name),
            2 => Path::new("2").join(&name),
            3 => Path::new("3").join(&name[..1]).join(&name),
            _ => Path::new(&name[0..2]).join(&name[2..4]).join(&name),
        }
    }

    fn commit_and_push(
        &self,
        msg: &str,
        modified_file: &Path,
        credentials: Option<(&str, &str)>,
    ) -> CargoResult<()> {
        // git add $file
        let mut index = self.repository.index()?;
        index.add_path(modified_file)?;
        index.write()?;
        let tree_id = index.write_tree()?;
        let tree = self.repository.find_tree(tree_id)?;

        // git commit -m "..."
        let head = self.repository.head()?;
        let parent = self.repository.find_commit(head.target().unwrap())?;
        let sig = self.repository.signature()?;
        self.repository
            .commit(Some("HEAD"), &sig, &sig, &msg, &tree, &[&parent])?;

        // git push
        let mut ref_status = Ok(());
        {
            let mut origin = self.repository.find_remote("origin")?;
            let mut callbacks = git2::RemoteCallbacks::new();
            callbacks.credentials(|_, _, _| {
                credentials
                    .ok_or_else(|| git2::Error::from_str("no authentication set"))
                    .and_then(|(u, p)| git2::Cred::userpass_plaintext(u, p))
            });
            callbacks.push_update_reference(|refname, status| {
                assert_eq!(refname, "refs/heads/master");
                if let Some(s) = status {
                    ref_status = Err(internal(&format_args!("failed to push a ref: {}", s)))
                }
                Ok(())
            });
            let mut opts = git2::PushOptions::new();
            opts.remote_callbacks(callbacks);
            origin.push(&["refs/heads/master"], Some(&mut opts))?;
        }
        ref_status
    }

    pub fn reset_head(&self) -> CargoResult<()> {
        let mut origin = self.repository.find_remote("origin")?;
        origin.fetch(&["refs/heads/*:refs/heads/*"], None, None)?;
        let head = self.repository.head()?.target().unwrap();
        let obj = self.repository.find_object(head, None)?;
        self.repository.reset(&obj, git2::ResetType::Hard, None)?;
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
pub struct AddCrate {
    krate: Crate,
}

impl Job for AddCrate {
    type Environment = Environment;
    const JOB_TYPE: &'static str = "add_crate";

    fn perform(self, env: &Self::Environment) -> CargoResult<()> {
        let repo = env.lock_index()?;
        let dst = repo.index_file(&self.krate.name);

        // Add the crate to its relevant file
        fs::create_dir_all(dst.parent().unwrap())?;
        let mut file = OpenOptions::new().append(true).create(true).open(&dst)?;
        serde_json::to_writer(&mut file, &self.krate)?;
        file.write_all(b"\n")?;

        repo.commit_and_push(
            &format!("Updating crate `{}#{}`", self.krate.name, self.krate.vers),
            &repo.relative_index_file(&self.krate.name),
            env.credentials(),
        )
    }
}

pub fn add_crate(conn: &PgConnection, krate: Crate) -> CargoResult<()> {
    AddCrate { krate }.enqueue(conn).map_err(Into::into)
}

#[derive(Serialize, Deserialize)]
pub struct Yank {
    krate: String,
    version: Version,
    yanked: bool,
}

impl Job for Yank {
    type Environment = Environment;
    const JOB_TYPE: &'static str = "yank";

    fn perform(self, env: &Self::Environment) -> CargoResult<()> {
        let repo = env.lock_index()?;
        let dst = repo.index_file(&self.krate);

        let conn = env.connection()?;

        conn.transaction(|| {
            let yanked_in_db = versions::table
                .find(self.version.id)
                .select(versions::yanked)
                .for_update()
                .first::<bool>(&*conn)?;

            if yanked_in_db == self.yanked {
                // The crate is alread in the state requested, nothing to do
                return Ok(());
            }

            let prev = fs::read_to_string(&dst)?;
            let version = self.version.num.to_string();
            let new = prev
                .lines()
                .map(|line| {
                    let mut git_crate = serde_json::from_str::<Crate>(line)
                        .map_err(|_| internal(&format_args!("couldn't decode: `{}`", line)))?;
                    if git_crate.name != self.krate || git_crate.vers != version {
                        return Ok(line.to_string());
                    }
                    git_crate.yanked = Some(self.yanked);
                    Ok(serde_json::to_string(&git_crate)?)
                })
                .collect::<CargoResult<Vec<String>>>();
            let new = new?.join("\n") + "\n";
            fs::write(&dst, new.as_bytes())?;

            repo.commit_and_push(
                &format!(
                    "{} crate `{}#{}`",
                    if self.yanked { "Yanking" } else { "Unyanking" },
                    self.krate,
                    self.version.num
                ),
                &repo.relative_index_file(&self.krate),
                env.credentials(),
            )?;

            diesel::update(&self.version)
                .set(versions::yanked.eq(self.yanked))
                .execute(&*conn)?;

            Ok(())
        })
    }
}

/// Yanks or unyanks a crate version. This requires finding the index
/// file, deserlialise the crate from JSON, change the yank boolean to
/// `true` or `false`, write all the lines back out, and commit and
/// push the changes.
pub fn yank(conn: &PgConnection, krate: String, version: Version, yanked: bool) -> CargoResult<()> {
    Yank {
        krate,
        version,
        yanked,
    }
    .enqueue(conn)
    .map_err(Into::into)
}
