//! Implementation of the `generate` command.

use crate::{
    common::{DocsetEntry, EntryType, Package},
    error::*
};

use cargo::{
    core::{compiler::CompileMode, Workspace},
    ops::{clean, CleanOptions, doc, CompileFilter, CompileOptions, DocOptions, FilterRule, LibRule, Packages},
    Config as CargoConfig
};
use rusqlite::{params, Connection};
use snafu::ResultExt;

use std::{
    borrow::ToOwned,
    ffi::OsStr,
    fs::{copy, create_dir_all, read_dir, remove_dir_all, File},
    io::Write,
    path::{Path, PathBuf}
};

#[derive(Debug)]
pub struct GenerateConfig {
    pub package: Package,
    pub no_dependencies: bool,
    pub doc_private_items: bool,
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub all_features: bool,
    pub exclude: Vec<String>,
    pub clean: bool,
    pub lib: bool,
    pub bins: Option<Vec<String>>
}

impl Default for GenerateConfig {
    fn default() -> GenerateConfig {
        GenerateConfig {
            package: Package::Current,
            no_dependencies: false,
            doc_private_items: false,
            exclude: Vec::new(),
            features: Vec::new(),
            no_default_features: true,
            all_features: false,
            clean: true,
            lib: false,
            bins: None
        }
    }
}

fn parse_docset_entry<P1: AsRef<Path>, P2: AsRef<Path>>(
    module_path: &Option<&str>,
    rustdoc_root_dir: P1,
    file_path: P2
) -> Option<DocsetEntry> {
    if file_path.as_ref().extension() == Some(OsStr::new("html")) {
        let file_name = file_path.as_ref().file_name().unwrap().to_string_lossy();
        let parts = file_name.split('.').collect::<Vec<_>>();

        let file_db_path = file_path
            .as_ref()
            .strip_prefix(&rustdoc_root_dir)
            .unwrap()
            .to_owned();
        match parts.len() {
            2 => {
                match parts[0] {
                    "index" => {
                        if let Some(mod_path) = module_path {
                            if mod_path.contains(':') {
                                // Module entry
                                Some(DocsetEntry::new(
                                    format!("{}::{}", mod_path, parts[0]),
                                    EntryType::Module,
                                    file_db_path
                                ))
                            } else {
                                // Package entry
                                Some(DocsetEntry::new(
                                    mod_path.to_string(),
                                    EntryType::Package,
                                    file_db_path
                                ))
                            }
                        } else {
                            None
                        }
                    }
                    _ => None
                }
            }
            3 => match parts[0] {
                "const" => Some(DocsetEntry::new(
                    format!("{}::{}", module_path.unwrap().to_string(), parts[1]),
                    EntryType::Constant,
                    file_db_path
                )),
                "enum" => Some(DocsetEntry::new(
                    format!("{}::{}", module_path.unwrap().to_string(), parts[1]),
                    EntryType::Enum,
                    file_db_path
                )),
                "fn" => Some(DocsetEntry::new(
                    format!("{}::{}", module_path.unwrap().to_string(), parts[1]),
                    EntryType::Function,
                    file_db_path
                )),
                "macro" => Some(DocsetEntry::new(
                    format!("{}::{}", module_path.unwrap().to_string(), parts[1]),
                    EntryType::Macro,
                    file_db_path
                )),
                "trait" => Some(DocsetEntry::new(
                    format!("{}::{}", module_path.unwrap().to_string(), parts[1]),
                    EntryType::Trait,
                    file_db_path
                )),
                "struct" => Some(DocsetEntry::new(
                    format!("{}::{}", module_path.unwrap().to_string(), parts[1]),
                    EntryType::Struct,
                    file_db_path
                )),
                "type" => Some(DocsetEntry::new(
                    format!("{}::{}", module_path.unwrap().to_string(), parts[1]),
                    EntryType::Type,
                    file_db_path
                )),
                _ => None
            },
            _ => None
        }
    } else {
        None
    }
}

const ROOT_SKIP_DIRS: &[&str] = &["src", "implementors"];

fn recursive_walk(
    root_dir: &Path,
    cur_dir: &Path,
    module_path: Option<&str>
) -> Result<Vec<DocsetEntry>> {
    let dir = read_dir(cur_dir).context(IoRead)?;
    let mut entries = vec![];
    let mut subdir_entries = vec![];

    for dir_entry in dir {
        let dir_entry = dir_entry.unwrap();
        if dir_entry.file_type().unwrap().is_dir() {
            let mut subdir_module_path =
                module_path.map(|p| format!("{}::", p)).unwrap_or_default();
            let dir_name = dir_entry.file_name().to_string_lossy().to_string();

            // Ignore some of the root directories which are of no interest to us
            if !(module_path.is_none() && ROOT_SKIP_DIRS.contains(&dir_name.as_str())) {
                subdir_module_path.push_str(&dir_name);
                subdir_entries.push(recursive_walk(
                    &root_dir,
                    &dir_entry.path(),
                    Some(&subdir_module_path)
                ));
            }
        } else if let Some(entry) = parse_docset_entry(&module_path, &root_dir, &dir_entry.path()) {
            entries.push(entry);
        }
    }
    for v in subdir_entries {
        entries.extend(v?);
    }
    Ok(entries)
}

fn generate_sqlite_index<P: AsRef<Path>>(docset_dir: P, entries: Vec<DocsetEntry>) -> Result<()> {
    let mut conn_path = docset_dir.as_ref().to_owned();
    conn_path.push("Contents");
    conn_path.push("Resources");
    conn_path.push("docSet.dsidx");
    let mut conn = Connection::open(&conn_path).context(Sqlite)?;
    conn.execute(
        "CREATE TABLE searchIndex(id INTEGER PRIMARY KEY, name TEXT, type TEXT, path TEXT);
        CREATE UNIQUE INDEX anchor ON searchIndex (name, type, path);
        )",
        params![]
    )
    .context(Sqlite)?;
    let transaction = conn.transaction().context(Sqlite)?;
    {
        let mut stmt = transaction
            .prepare("INSERT INTO searchIndex (name, type, path) VALUES (?1, ?2, ?3)")
            .context(Sqlite)?;
        for entry in entries {
            stmt.execute(&[
                entry.name,
                entry.ty.to_string(),
                entry.path.to_str().unwrap().to_owned()
            ])
            .context(Sqlite)?;
        }
    }
    transaction.commit().context(Sqlite)?;
    Ok(())
}

fn copy_dir_recursive<Ps: AsRef<Path>, Pd: AsRef<Path>>(src: Ps, dst: Pd) -> Result<()> {
    create_dir_all(&dst).context(IoWrite)?;
    for entry in read_dir(&src).context(IoRead)? {
        let entry = entry.context(IoWrite)?.path();
        if entry.is_dir() {
            let mut dst_dir = dst.as_ref().to_owned();
            dst_dir.push(entry.strip_prefix(&src).unwrap());
            copy_dir_recursive(entry, dst_dir)?;
        } else if entry.is_file() {
            let mut dst_file = dst.as_ref().to_owned();
            dst_file.push(entry.file_name().unwrap());
            copy(entry, dst_file).context(IoWrite)?;
        }
    }
    Ok(())
}

fn write_metadata<P: AsRef<Path>>(docset_root_dir: P, package_name: &str) -> Result<()> {
    let mut info_plist_path = docset_root_dir.as_ref().to_owned();
    info_plist_path.push("Contents");
    info_plist_path.push("Info.plist");

    let mut info_file = File::create(info_plist_path).context(IoWrite)?;
    write!(info_file,
        "\
        <?xml version=\"1.0\" encoding=\"UTF-8\"?>
        <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
        <plist version=\"1.0\">
        <dict>
            <key>CFBundleIdentifier</key>
                <string>{}</string>
            <key>CFBundleName</key>
                <string>{}</string>
            <key>dashIndexFilePath</key>
                <string>{}/index.html</string>
            <key>DocSetPlatformFamily</key>
                <string>{}</string>
            <key>isDashDocset</key>
                <true/>
        </dict>
        </plist>",
         package_name, package_name, package_name, package_name).context(IoWrite)?;
    Ok(())
}

pub fn generate(cargo_cfg: &CargoConfig, workspace: &Workspace, cfg: GenerateConfig) -> Result<()> {
    // Step 1: generate rustdoc
    // Figure out for which crate to build the doc and invoke cargo doc.
    // If no crate is specified, run cargo doc for the current crate/workspace.
    let mut compile_opts = CompileOptions::new(
        &cargo_cfg,
        CompileMode::Doc {
            deps: !cfg.no_dependencies
        }
    ).context(CargoDoc)?;
    compile_opts.all_features = cfg.all_features;
    compile_opts.no_default_features = cfg.no_default_features;
    compile_opts.features = cfg.features;
    if cfg.lib || cfg.bins.is_some() {
        let bins_filter_rule =
            if let Some(bins) = cfg.bins {
                if bins.is_empty() {
                    FilterRule::All
                }
                else {
                    FilterRule::Just(bins)
                }
            }
            else { FilterRule::Just(vec![]) };
        compile_opts.filter = CompileFilter::Only {
            all_targets: false,
            lib: if cfg.lib { LibRule::True } else { LibRule::Default },
            bins: bins_filter_rule,
            examples: FilterRule::Just(vec![]),
            tests: FilterRule::Just(vec![]),
            benches: FilterRule::Just(vec![]),
        }
    }
    if cfg.doc_private_items {
        compile_opts.local_rustdoc_args = Some(vec!["--document-private-items".to_owned()]);
    }
    let root_package_name = match &cfg.package {
        Package::All => {
            if cfg.exclude.is_empty() {
                compile_opts.spec = Packages::All;
            } else {
                compile_opts.spec = Packages::OptOut(cfg.exclude.clone());
            }
            workspace
                .root()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        }
        Package::Current => {
            compile_opts.spec = Packages::Default;
            workspace
                .current()
                .context(Cargo)?
                .name()
                .as_str()
                .to_owned()
        }
        Package::Single(name) => {
            compile_opts.spec = Packages::Packages(vec![name.clone()]);
            name.to_owned()
        }
        Package::List(packages) => {
            compile_opts.spec = Packages::Packages(packages.clone());
            workspace
                .root()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        }
    };
    if cfg.package != Package::All && !cfg.exclude.is_empty() {
        return Args {
            msg: "--exclude must be used with --all"
        }
        .fail();
    }
    let mut docset_root_dir = PathBuf::new();
    docset_root_dir.push(workspace.root());
    docset_root_dir.push("target");
    let mut rustdoc_root_dir = docset_root_dir.clone();
    rustdoc_root_dir.push("doc");
    docset_root_dir.push("docset");
    docset_root_dir.push(format!("{}.docset", root_package_name));

    if cfg.clean {
        let clean_options = CleanOptions { config: &cargo_cfg, spec: vec![], target: None, release: false, doc: true };
        clean(&workspace, &clean_options).context(CargoClean)?;
    }
    // Good to go, generate the documentation.
    let doc_cfg = DocOptions {
        open_result: false,
        compile_opts
    };
    doc(&workspace, &doc_cfg).context(CargoDoc)?;

    // Step 2: iterate over all the html files in the doc directory and parse the filenames
    let entries = recursive_walk(&rustdoc_root_dir, &rustdoc_root_dir, None)?;

    // Step 3: generate the SQLite database
    // At this point, we need to start writing into the output docset directory, so create the
    // hirerarchy, and clean it first if it already exists.
    if docset_root_dir.exists() {
        remove_dir_all(&docset_root_dir).context(IoWrite)?;
    }
    let mut docset_hierarchy = docset_root_dir.clone();
    docset_hierarchy.push("Contents");
    docset_hierarchy.push("Resources");
    create_dir_all(&docset_hierarchy).context(IoWrite)?;
    generate_sqlite_index(&docset_root_dir, entries)?;

    // Step 4: Copy the rustdoc to the docset directory
    docset_hierarchy.push("Documents");
    copy_dir_recursive(&rustdoc_root_dir, &docset_hierarchy)?;

    // Step 5: add the required metadata
    write_metadata(&docset_root_dir, &root_package_name)?;

    Ok(())
}
