//! Compiler-side inventory and explicit target selection for project workers.
use std::{io::Write, path::PathBuf, process::Command};

use anyhow::{Context, ensure};
use clap::Args;
use rustc_middle::ty::TyCtxt;
use rustc_span::{FileName, RemapPathScopeComponents};
use rustc_utils::source_map::find_bodies::find_bodies;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::plugin::{FlowistryError, FlowistryResult};

#[derive(Default, Args, Serialize, Deserialize)]
pub(crate) struct Selection {
  #[arg(long, requires_all = ["target_kind", "target_name"])]
  pub package: Option<String>,
  #[arg(long, requires = "package", value_parser = ["lib", "bin", "example", "test", "bench"])]
  pub target_kind: Option<String>,
  #[arg(long, requires = "package")]
  pub target_name: Option<String>,
  #[arg(long)]
  pub features: Option<String>,
  #[arg(long)]
  pub all_features: bool,
  #[arg(long)]
  pub no_default_features: bool,
}

impl Selection {
  pub(crate) fn append_args(&self, args: &mut Vec<String>) {
    for (flag, value) in [
      ("--package", &self.package),
      ("--target-kind", &self.target_kind),
      ("--target-name", &self.target_name),
      ("--features", &self.features),
    ] {
      if let Some(value) = value {
        args.extend([flag.into(), value.clone()]);
      }
    }
    if self.all_features {
      args.push("--all-features".into());
    }
    if self.no_default_features {
      args.push("--no-default-features".into());
    }
  }

  fn metadata(&self) -> anyhow::Result<Value> {
    let output = Command::new("cargo")
      .args([
        "metadata",
        "--no-deps",
        "--offline",
        "--format-version",
        "1",
      ])
      .output()?;
    ensure!(
      output.status.success(),
      "Cargo metadata failed: {}",
      String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
  }

  fn selected_target(&self) -> anyhow::Result<(String, String)> {
    let metadata = self.metadata()?;
    let packages = metadata["packages"]
      .as_array()
      .context("missing Cargo packages")?;
    let members = metadata["workspace_members"]
      .as_array()
      .context("missing workspace members")?;
    let package = self
      .package
      .as_deref()
      .context("explicit package required")?;
    let matches = packages
      .iter()
      .filter(|p| {
        members.contains(&p["id"])
          && (p["name"].as_str() == Some(package) || p["id"].as_str() == Some(package))
      })
      .collect::<Vec<_>>();
    ensure!(
      matches.len() == 1,
      "package must identify exactly one workspace member: {package}"
    );
    let kind = self
      .target_kind
      .as_deref()
      .context("target kind required")?;
    let name = self
      .target_name
      .as_deref()
      .context("target name required")?;
    let target = matches[0]["targets"]
      .as_array()
      .context("missing targets")?
      .iter()
      .find(|t| {
        t["name"].as_str() == Some(name)
          && t["kind"].as_array().is_some_and(|kinds| {
            kinds.iter().any(|k| {
              k.as_str() == Some(kind)
                || (kind == "lib"
                  && matches!(
                    k.as_str(),
                    Some("rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro")
                  ))
            })
          })
      })
      .context("requested target does not exist in the selected package")?;
    let crate_types = target["crate_types"]
      .as_array()
      .context("missing crate types")?;
    ensure!(
      crate_types.len() == 1,
      "project workers currently require a single crate type per target"
    );
    Ok((
      name.replace('-', "_"),
      crate_types[0]
        .as_str()
        .context("invalid crate type")?
        .into(),
    ))
  }

  pub fn modify_cargo(&self, cargo: &mut Command) {
    let selection = if self.package.is_some() {
      self.explicit_cargo(cargo)
    } else {
      self.inferred_cargo(cargo)
    };
    if let Err(error) = selection {
      eprintln!("flowistry: {error:#}");
      std::process::exit(2);
    }
    if let Some(features) = &self.features {
      cargo.args(["--features", features]);
    }
    if self.all_features {
      cargo.arg("--all-features");
    }
    if self.no_default_features {
      cargo.arg("--no-default-features");
    }
  }

  fn inferred_cargo(&self, cargo: &mut Command) -> anyhow::Result<()> {
    // rustc_plugin chooses the Cargo target, but uses the package name for
    // SPECIFIC_CRATE on libraries and binaries. Cargo permits distinct names.
    // Resolve the already selected target rather than guessing from the file.
    let args = cargo
      .get_args()
      .map(|a| a.to_string_lossy())
      .collect::<Vec<_>>();
    let package = args
      .windows(2)
      .find(|p| p[0] == "-p")
      .context("missing selected Cargo package")?;
    let metadata = self.metadata()?;
    let package = metadata["packages"]
      .as_array()
      .context("missing Cargo packages")?
      .iter()
      .find(|p| {
        format!(
          "{}:{}",
          p["name"].as_str().unwrap_or_default(),
          p["version"].as_str().unwrap_or_default()
        ) == package[1]
      })
      .context("selected Cargo package not found")?;
    let named = args
      .windows(2)
      .find(|p| matches!(p[0].as_ref(), "--bin" | "--example" | "--test" | "--bench"));
    let target = package["targets"]
      .as_array()
      .context("missing Cargo targets")?
      .iter()
      .find(|t| {
        let kinds = t["kind"].as_array();
        if let Some(pair) = named {
          t["name"].as_str() == Some(pair[1].as_ref())
            && kinds
              .is_some_and(|ks| ks.iter().any(|k| k.as_str() == Some(&pair[0][2 ..])))
        } else {
          kinds.is_some_and(|ks| {
            ks.iter().any(|k| {
              matches!(
                k.as_str(),
                Some("lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro")
              )
            })
          })
        }
      })
      .context("selected Cargo target not found")?;
    let name = target["name"]
      .as_str()
      .context("missing target name")?
      .replace('-', "_");
    cargo.env("SPECIFIC_CRATE", &name);
    // A previous ordinary check (including one with the wrong crate filter)
    // may have left metadata behind. Ensure the selected compiler runs again.
    Self::invalidate_target(cargo, &name)?;
    Ok(())
  }

  fn invalidate_target(cargo: &Command, crate_name: &str) -> anyhow::Result<()> {
    let args = cargo.get_args().collect::<Vec<_>>();
    let directory = args
      .windows(2)
      .find(|pair| pair[0] == "--target-dir")
      .map(|pair| PathBuf::from(pair[1]).join("debug/deps"))
      .context("missing plugin target directory")?;
    if let Ok(entries) = std::fs::read_dir(directory) {
      let prefix = format!("lib{crate_name}-");
      for entry in entries {
        let path = entry?.path();
        if path
          .file_name()
          .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
          && matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("rmeta" | "rlib")
          )
        {
          std::fs::remove_file(path)?;
        }
      }
    }
    Ok(())
  }

  fn explicit_cargo(&self, cargo: &mut Command) -> anyhow::Result<()> {
    let (crate_name, crate_type) = self.selected_target()?;
    // rustc_plugin selected OnlyWorkspace to bypass its path heuristic. Preserve
    // its wrapper/environment/target directory, replacing only workspace selection.
    let mut command = Command::new(cargo.get_program());
    command.args(cargo.get_args().filter(|arg| *arg != "--all"));
    for (key, value) in cargo.get_envs() {
      if let Some(value) = value {
        command.env(key, value);
      } else {
        command.env_remove(key);
      }
    }
    if let Some(cwd) = cargo.get_current_dir() {
      command.current_dir(cwd);
    }
    command.arg("-p").arg(self.package.as_ref().unwrap());
    let kind = self.target_kind.as_deref().unwrap();
    if kind == "lib" {
      command.arg("--lib");
    } else {
      command
        .arg(format!("--{kind}"))
        .arg(self.target_name.as_ref().unwrap());
    }
    command
      .env("SPECIFIC_CRATE", &crate_name)
      .env("SPECIFIC_TARGET", crate_type);
    Self::invalidate_target(&command, &crate_name)?;
    *cargo = command;
    Ok(())
  }
}

struct Inventory {
  output: Option<FlowistryResult<Value>>,
  mode: flowistry::extensions::EvalMode,
}

/// Cargo remains responsible for workspace and target identity. Editors never
/// need to guess a package name from a directory or invoke a different Cargo.
pub(crate) fn targets() -> anyhow::Result<Value> {
  let metadata = Selection::default().metadata()?;
  let members = metadata["workspace_members"]
    .as_array()
    .context("missing workspace members")?;
  let mut targets = Vec::new();
  for package in metadata["packages"]
    .as_array()
    .context("missing packages")?
  {
    if !members.contains(&package["id"]) {
      continue;
    }
    for target in package["targets"].as_array().context("missing targets")? {
      let kinds = target["kind"].as_array().context("missing target kind")?;
      let kind = if kinds.iter().any(|kind| {
        matches!(
          kind.as_str(),
          Some("lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro")
        )
      }) {
        "lib"
      } else if let Some(kind) = kinds.iter().find_map(|kind| match kind.as_str() {
        Some(kind @ ("bin" | "example" | "test" | "bench")) => Some(kind),
        _ => None,
      }) {
        kind
      } else {
        continue;
      };
      targets.push(json!({
        "package": package["name"], "package_id":package["id"],
        "manifest_path": package["manifest_path"], "target_kind":kind,
        "target_name":target["name"], "src_path":target["src_path"],
        "required_features":target.get("required-features").unwrap_or(&json!([])),
        "supported":target["crate_types"].as_array().is_some_and(|types| types.len() == 1),
      }));
    }
  }
  targets.sort_by_key(|t| {
    (
      t["package"].as_str().unwrap_or_default().to_owned(),
      t["target_kind"].as_str().unwrap_or_default().to_owned(),
      t["target_name"].as_str().unwrap_or_default().to_owned(),
    )
  });
  Ok(json!({"schema":1, "workspace_root":metadata["workspace_root"], "targets":targets}))
}

impl rustc_driver::Callbacks for Inventory {
  fn after_expansion<'tcx>(
    &mut self,
    _compiler: &rustc_interface::interface::Compiler,
    tcx: TyCtxt<'tcx>,
  ) -> rustc_driver::Compilation {
    let source_map = tcx.sess.source_map();
    let mut bodies = Vec::new();
    let mut expanded = std::collections::BTreeMap::new();
    let cache_enabled = crate::cache::store_root().is_some();
    for (span, id) in find_bodies(tcx) {
      let def = tcx.hir_body_owner_def_id(id);
      let identity = format!("{:?}", tcx.def_path_hash(def.to_def_id()));
      let name = tcx.def_path_str(def);
      expanded.insert(
        identity.clone(),
        if cache_enabled {
          crate::cache::expanded_body(tcx, def)
        } else {
          String::new()
        },
      );
      let result = (|| -> anyhow::Result<Value> {
        let file = source_map.lookup_source_file(span.lo());
        let FileName::Real(file_name) = &file.name else {
          anyhow::bail!("body has no local source file");
        };
        let filename = file_name.path(RemapPathScopeComponents::DOCUMENTATION);
        let path = filename
          .canonicalize()
          .context("body source is not accessible on disk")?;
        let filename = path.to_str().context("body source path is not UTF-8")?;
        let range = crate::positions::char_range(span, source_map)?;
        Ok(serde_json::to_value(crate::fast_cache::BodyIdentity::new(
          tcx, id, &range, filename,
        ))?)
      })();
      bodies.push(match result {
        Ok(body) => body,
        Err(error) => {
          json!({"identity": identity, "name": name, "error": error.to_string()})
        }
      });
    }
    bodies.sort_by_key(|body| body["identity"].as_str().unwrap_or_default().to_owned());
    let mode = self.mode;
    let plan = crate::save_plan::inventory(
      &if cache_enabled {
        crate::cache::context(tcx)
      } else {
        String::new()
      },
      &format!("{mode:?}"),
      mode.context_mode == flowistry::extensions::ContextMode::Recurse,
      &expanded,
    );
    for body in &mut bodies {
      body["save_plan"] = plan[body["identity"].as_str().unwrap()].clone();
    }
    self.output = Some(Ok(json!({"schema": 1, "bodies": bodies})));
    if tcx.dcx().has_errors().is_none() {
      if crate::plugin::postprocess(self.output.take().unwrap()).is_ok() {
        std::io::stdout().flush().unwrap();
        std::process::exit(0);
      }
    }
    rustc_driver::Compilation::Stop
  }
}

pub(crate) fn discover(args: &[String]) -> FlowistryResult<Value> {
  let mut callbacks = Inventory {
    output: None,
    mode: flowistry::extensions::EVAL_MODE
      .copied()
      .unwrap_or_default(),
  };
  crate::plugin::run_with_callbacks(args, &mut callbacks)?;
  callbacks.output.unwrap_or_else(|| {
    Err(FlowistryError::AnalysisError {
      error: "compiler did not enumerate bodies".into(),
    })
  })
}
