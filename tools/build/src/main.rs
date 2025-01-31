use std::{collections::{BTreeMap, BTreeSet}, fs, io::{ErrorKind, Write as _}, path::{Path, PathBuf}, sync::Arc, time::Instant};

use clap::Parser;
use miette::{miette, Context, IntoDiagnostic as _, LabeledSpan, NamedSource};
use rangemap::RangeMap;
use size::Size;
use hubris_build::{alloc::allocate_space, appcfg, buildid::BuildId, cargo::{do_cargo_build, LinkStyle}, get_target_spec, relink::{collect_sizes, merge_ranges, relink_final, relink_for_size}, verbose::{banner, print_allocations, simple_table}};
use hubris_region_alloc::{Mem, TaskInfo, TaskName};

#[derive(Parser)]
struct Tool {
    #[clap(subcommand)]
    cmd: Cmd,

    #[clap(short, long, global=true)]
    verbose: bool,
}

#[derive(Parser)]
enum Cmd {
    /// Builds a Hubris application, given a path to its config.
    Build {
        /// Path to the application definition.
        cfg_path: PathBuf,

        /// Cause Cargo to become very chatty (passes the `--verbose` flag to
        /// all Cargo commands)
        #[clap(long)]
        cargo_verbose: bool,

        /// Overrides the output bundle path, if you'd like it placed somewhere
        /// specific. Defaults to APPNAME-build.zip in the project root.
        #[clap(short, long)]
        out: Option<PathBuf>,
    },
    /// Assembles a .hex and (optionally) gdbconfig from build products.
    PackHex {
        /// Final output directory (e.g. .work/appnamehere/final)
        bindir: PathBuf,
        /// Filename for .hex file.
        outpath: PathBuf,

        /// If provided, requests a gdbconfig be generated at the given path.
        #[clap(short, long)]
        gdbconfig: Option<PathBuf>,
    },
    /// Reads and performs basic checks on an IDL file.
    CheckIdl {
        path: PathBuf,
    },
    /// Produces an output bundle from raw files on disk, which can be useful if
    /// you're modifying the bundle somehow.
    Bundle {
        /// Path to the appcfg, which should correspond to the output files.
        cfg_path: PathBuf,
        /// Final output directory (e.g. .work/appnamehere/final)
        bindir: PathBuf,
        /// Output path for constructed bundle.
        outpath: PathBuf,
    },
}

fn main() -> miette::Result<()> {
    let args = Tool::parse();

    match args.cmd {

        // The Build Command
        Cmd::Build { cfg_path, cargo_verbose, out } => {
            // Canonicalize directories and locate/parse input files.
            let root = std::env::var("HUBRIS_PROJECT_ROOT").into_diagnostic()?;
            let root = PathBuf::from(root);

            let doc_src = fs::read_to_string(&cfg_path)
                .into_diagnostic()
                .wrap_err_with(|| format!("can't read {}", cfg_path.display()))?;
            let source = Arc::new(NamedSource::new(
                cfg_path.display().to_string(),
                doc_src.clone(),
            ));
            let doc: kdl::KdlDocument = appcfg::add_source(&source, || {
                Ok(doc_src.parse()?)
            })?;
            let ctx = appcfg::FsContext::from_root(&root);
            let app = appcfg::parse_app(source, &doc, &ctx)?;

            // Begin building our BuildId.
            let mut buildid = BuildId::new();

            // See if we understand this target.
            let target_spec = get_target_spec(app.board.chip.target_triple.value())
                .ok_or_else(|| {
                    miette!(
                        labels = [LabeledSpan::at(
                            app.board.chip.target_triple.span(),
                            "triple defined here",
                        )],
                        "target spec not defined for triple"
                    )
                })?;
            buildid.hash(&target_spec);

            // Interrogate the installed toolchain to discover things like the
            // linker path.
            let env = hubris_build::determine_build_env()?;

            buildid.hash(&env);

            // Print the kickoff header:
            simple_table([
                ("App name", app.name.value().as_str()),
                ("Config path", &cfg_path.display().to_string()),
                ("Workspace root", &root.display().to_string()),
                ("Host platform", &env.host_triple),
                ("Default toolchain", &env.release),
            ]);

            // Analyze the app and make a build plan.
            let overall_plan = hubris_build::appcfg::plan_build(&app)?;
            // Locate the workspace Cargo target directory.
            //
            // TODO: currently this assumes that the project root is also the
            // workspace root; this is not necessarily true? TBD.
            let targetroot = root.join("target");
            // Create our working directory.
            let workdir = root.join(".work").join(app.name.value());
            maybe_create_dir(&workdir).into_diagnostic()?;

            // Create our tmp directory, which we'll use for short-lived files
            // during the build process.
            let tmpdir = workdir.join("tmp");
            maybe_create_dir(&tmpdir).into_diagnostic()?;

            // Create our initial build directory, where we deposit
            // partially-linked task binaries.
            let initial_build_dir = workdir.join("build");
            maybe_create_dir(&initial_build_dir).into_diagnostic()?;

            // Run the initial build of each task using the task-rlink script.
            let task_rlink_text = include_str!("../../../files/task-rlink.x");
            buildid.eat(task_rlink_text.as_bytes());
            for (name, plan) in &overall_plan.tasks {
                do_cargo_build(
                    task_rlink_text,
                    plan,
                    LinkStyle::Partial,
                    &targetroot,
                    &tmpdir,
                    &initial_build_dir,
                    name,
                    cargo_verbose,
                    args.verbose,
                )?;
            }

            // We now have partially-linked ELF files in initial_build_dir.

            // Begin the second link phase...
            banner("Task build complete, prelinking for size...");
            let temp_link_dir = workdir.join("link2");
            maybe_create_dir(&temp_link_dir).into_diagnostic()?;
            let task_link2_text = include_str!("../../../files/task-link2.x");
            buildid.eat(task_link2_text.as_bytes());
            std::fs::write(workdir.join("task-link2.x"), task_link2_text).into_diagnostic()?;

            // Start building up a map of size requirements.
            let mut size_reqs: BTreeMap<TaskName, TaskInfo> = BTreeMap::new();

            // Relink each task independently.
            for taskname in overall_plan.tasks.keys() {
                let region_sizes = relink_for_size(
                    &env,
                    &app,
                    &target_spec,
                    &targetroot,
                    &app.tasks[taskname],
                    &initial_build_dir.join(taskname),
                    &temp_link_dir.join(taskname),
                    &workdir.join("task-link2.x"),
                )?;

                let ti = size_reqs.entry(TaskName(taskname.clone())).or_default();
                for (region, rangeset) in region_sizes {
                    if let Some(range) = merge_ranges(&rangeset) {
                        let size = range.end - range.start;
                        ti.reqs.insert(Mem(region), size);
                    }
                }
                ti.regs_avail = target_spec.region_count - app.tasks[taskname].peripherals.len();
            }

            // Run the global memory allocator. Because the allocator uses
            // algorithms with potentially poor scaling behavior, we time how
            // long it takes.
            let alloc_begin = Instant::now();
            let allocs = allocate_space(&target_spec, &app.board.chip.memory, &size_reqs, &app.kernel)?;
            let alloc_time = alloc_begin.elapsed();

            buildid.hash(&allocs);

            // Begin our final link phase by clearing out any cruft.
            let dir3 = workdir.join("final");
            match std::fs::remove_dir_all(&dir3) {
                Ok(()) => (),
                Err(e) if e.kind() == ErrorKind::NotFound => (),
                e => e.into_diagnostic()?,
            }
            maybe_create_dir(&dir3).into_diagnostic()?;

            let task_link3_text = include_str!("../../../files/task-link3.x");
            buildid.eat(task_link3_text.as_bytes());
            std::fs::write(workdir.join("task-link3.x"), task_link3_text).into_diagnostic()?;

            // Run the final link of each task and collect its built info.
            let mut built_tasks = vec![];
            for taskname in overall_plan.tasks.keys() {
                let built_task = relink_final(
                    &env,
                    &app,
                    &target_spec,
                    &targetroot,
                    &app.tasks[taskname],
                    &initial_build_dir.join(taskname),
                    &dir3.join(taskname),
                    &workdir.join("task-link3.x"),
                    &allocs.tasks[taskname],
                ).with_context(|| format!("failed final link for task {taskname}"))?;

                built_tasks.push(built_task);
            }

            // Use the task build info to generate the kconfig data structure.
            let kconfig = hubris_build::kconfig::generate_kconfig(&app, &built_tasks)?;

            buildid.hash(&kconfig);

            // Generate the kernel's linker script memory fragment into
            // {tmpdir}/memory.x. The kernel's MEMORY section will simply grant
            // it all remaining memory in each region.
            {
                let linker_script_path = tmpdir.join("memory.x");
                let mut scr = std::fs::File::create(&linker_script_path)
                    .into_diagnostic()?;
                writeln!(scr, "MEMORY {{").into_diagnostic()?;

                writeln!(scr, "STACK (rw): ORIGIN = {:#x}, LENGTH = {:#x}",
                    allocs.kernel.stack.start,
                    allocs.kernel.stack.end - allocs.kernel.stack.start,
                    ).into_diagnostic()?;

                for orig_name in app.board.chip.memory.keys() {
                    let Some(regalloc) = allocs.kernel.by_region.get(&Mem(orig_name.clone())) else {
                        continue;
                    };

                    let name = orig_name.to_ascii_uppercase();
                    let base = regalloc.start;
                    let size = regalloc.end - regalloc.start;

                    writeln!(scr, "{name} (rw): ORIGIN = {base:#x}, LENGTH = {size:#x}").into_diagnostic()?;
                }
                writeln!(scr, "}}").into_diagnostic()?;

                // TODO these values are hardcoded
                writeln!(scr, "_HUBRIS_IMAGE_HEADER_ALIGN = 4;").into_diagnostic()?;
                writeln!(scr, "_HUBRIS_IMAGE_HEADER_SIZE = 0x50;").into_diagnostic()?;
            }

            // Modify the original overall_plan by inserting our kconfig.
            let mut overall_plan = overall_plan;
            overall_plan.kernel.smuggled_env.insert(
                "HUBRIS_KCONFIG".to_string(),
                ron::ser::to_string(&kconfig).into_diagnostic()?,
            );

            // This is duplicative, but, whatever. People like it when the build
            // ID changes when other things change.
            buildid.hash(&overall_plan);

            // Generate the kernel linker script on disk and hash it, too.
            let kernel_link_text = include_str!("../../../files/kernel-link.x");
            buildid.eat(kernel_link_text.as_bytes());
            std::fs::write(workdir.join("kernel-link.x"), kernel_link_text).into_diagnostic()?;

            // Finalize the buildid and insert it into the kernel env.
            overall_plan.kernel.smuggled_env.insert(
                "HUBRIS_IMAGE_ID".to_string(),
                buildid.finish().to_string(),
            );

            // Build the actual kernel.
            do_cargo_build(
                kernel_link_text,
                &overall_plan.kernel,
                LinkStyle::Full,
                &targetroot,
                &tmpdir,
                &dir3,
                "kernel",
                cargo_verbose,
                args.verbose,
            )?;

            // Measure the kernel so we can report the actual allocations.
            {
                let kernel_image = std::fs::read(dir3.join("kernel")).into_diagnostic()?;
                let ksizes = collect_sizes(&app, &kernel_image)?;
                println!();
                println!("Allocations ({alloc_time:?}):");
                print_allocations(
                    &app,
                    &allocs.by_region(),
                    &ksizes,
                );
            }

            std::fs::remove_file(tmpdir.join("memory.x")).into_diagnostic()?;

            // Construct a bundle containing the output.
            let out = out.unwrap_or_else(|| {
                root.join(format!("{}-build.zip", app.name.value()))
            });
            hubris_build::bundle::make_bundle(
                &app,
                &dir3,
                &out,
            ).into_diagnostic()?;

            banner(format!("Build complete! Archive: {}", out.display()));

            Ok(())
        }

        // The PackHex Command
        Cmd::PackHex { bindir, outpath, gdbconfig } => {
            let mut overall_segments = RangeMap::new();
            let mut protohex = vec![];
            let mut start = None;
            let mut all_paths = vec![];

            for dirent in std::fs::read_dir(&bindir).into_diagnostic()? {
                let dirent = dirent.into_diagnostic()?;
                let name = dirent.file_name();
                let name = name.into_string().unwrap();
                let path = dirent.path();
                all_paths.push(path.clone());
                let bytes = std::fs::read(&path).into_diagnostic()?;
                let elf = goblin::elf::Elf::parse(&bytes).into_diagnostic()?;

                for phdr in &elf.program_headers {
                    if phdr.p_type == goblin::elf::program_header::PT_LOAD {
                        let arange = phdr.p_vaddr..phdr.p_vaddr + phdr.p_memsz;
                        if overall_segments.overlaps(&arange) {
                            panic!("range {arange:x?} for {name} overlaps another!");
                        }
                        overall_segments.insert(arange, name.clone());
                        if phdr.p_filesz != 0 {
                            let slice = &bytes[phdr.p_offset as usize..(phdr.p_offset + phdr.p_filesz) as usize];

                            for (i, chunk) in slice.chunks(255).enumerate() {
                                let addr = phdr.p_paddr + i as u64 * 255;
                                protohex.push((addr, chunk.to_vec()));
                            }
                        }
                    }
                }

                if name == "kernel" {
                    start = Some(elf.entry);
                }
            }

            protohex.sort_by_key(|(addr, _)| *addr);

            let mut hex = vec![];
            for (addr, data) in protohex {
                hex.push(ihex::Record::ExtendedLinearAddress((addr >> 16) as u16));
                hex.push(ihex::Record::Data {
                    offset: addr as u16,
                    value: data,
                });
            }
            if let Some(a) = start {
                hex.push(ihex::Record::StartLinearAddress(a as u32));
            }
            hex.push(ihex::Record::EndOfFile);

            let mut rows = vec![];
            for (range, entity) in overall_segments.iter() {
                rows.push((
                    range.start,
                    range.end,
                    entity.clone(),
                ));
            }
            let bottom = overall_segments.first_range_value().unwrap().0.start;
            let top = overall_segments.last_range_value().unwrap().0.end;
            for gap in overall_segments.gaps(&(bottom..top)) {
                rows.push((
                    gap.start,
                    gap.end,
                    "- unused -".to_string(),
                ));
            }
            rows.sort();
            let mut table = comfy_table::Table::new();
            table.load_preset(comfy_table::presets::NOTHING);
            table.set_header(["START", "END (ex)", "OWNER"]);
            table.column_mut(0).unwrap().set_cell_alignment(comfy_table::CellAlignment::Right);
            table.column_mut(1).unwrap().set_cell_alignment(comfy_table::CellAlignment::Right);
            for row in rows {
                table.add_row([
                    format!("{:#x}", row.0),
                    format!("{:#x}", row.1),
                    row.2,
                ]);
            }
            println!("{table}");

            let hexstr = ihex::create_object_file_representation(&hex).into_diagnostic()?;
            std::fs::write(outpath, &hexstr).into_diagnostic()?;

            if let Some(gdbconfig) = gdbconfig {
                let mut g = std::fs::File::create(gdbconfig).into_diagnostic()?;
                for p in &all_paths {
                    if p.ends_with("kernel") {
                        writeln!(g, "file {}", p.display()).into_diagnostic()?;
                    }
                }
                writeln!(g, "target extended-remote localhost:3333").into_diagnostic()?;
                for p in all_paths {
                    if !p.ends_with("kernel") {
                        writeln!(g, "add-symbol-file {}", p.display()).into_diagnostic()?;
                    }
                }
            }

            Ok(())
        }

        // The CheckIdl Command
        Cmd::CheckIdl { path } => {
            let interface = hubris_build::idl::load_interface(path)?;
            println!();
            println!("Got:");
            println!("{interface:#?}");

            let client = hubris_build::idl::codegen::generate_client(&interface)?;
            let client = hubris_build::idl::codegen::format_code(&client);
            println!();
            println!("Client:");
            println!("{client}");
            for (name, t) in &interface.types {
                match t {
                    hubris_build::idl::TypeDef::Enum(e) => {
                        let s = hubris_build::idl::codegen::generate_enum(name, e)?;
                        println!("{}", hubris_build::idl::codegen::format_code(&s));
                    }
                    hubris_build::idl::TypeDef::Struct(_) => (),
                }
            }
            Ok(())
        }

        // The Bundle Command
        Cmd::Bundle { cfg_path, bindir, outpath } => {
            // Canonicalize directories and locate/parse input files.
            let root = std::env::var("HUBRIS_PROJECT_ROOT").into_diagnostic()?;
            let root = PathBuf::from(root);
            let doc_src = fs::read_to_string(&cfg_path)
                .into_diagnostic()
                .wrap_err_with(|| format!("can't read {}", cfg_path.display()))?;
            let source = Arc::new(NamedSource::new(
                cfg_path.display().to_string(),
                doc_src.clone(),
            ));
            let doc: kdl::KdlDocument = appcfg::add_source(&source, || {
                Ok(doc_src.parse()?)
            })?;
            let ctx = appcfg::FsContext::from_root(&root);
            let app = appcfg::parse_app(source, &doc, &ctx)?;

            hubris_build::bundle::make_bundle(
                &app,
                &bindir,
                &outpath,
            ).into_diagnostic()?;

            let final_meta = std::fs::metadata(&outpath).into_diagnostic()?;
            println!("built {}: {} on disk", outpath.display(), Size::from_bytes(final_meta.len()));
            Ok(())
        }
    }
}

pub fn guess_intent<'a>(
    value: &str,
    valid_options: impl IntoIterator<Item = &'a String>,
) -> BTreeSet<&'a str> {
    const MAX: usize = 4;

    valid_options.into_iter()
        .filter_map(|option| if strsim::damerau_levenshtein(value, option) <= MAX {
            Some(option.as_str())
        } else {
            None
        })
        .collect()
}

fn maybe_create_dir(path: impl AsRef<Path>) -> std::io::Result<()> {
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}
