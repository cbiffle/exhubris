use std::ops::Range;
use std::{collections::BTreeMap, path::Path};
use std::io::Write;

use goblin::elf::program_header::PT_LOAD;
use miette::{IntoDiagnostic as _, miette};
use rangemap::RangeSet;

use crate::cmd_with_clean_env;
use crate::{alloc::TaskAllocation, appcfg::{AppDef, TaskDef}, BuildEnv, TargetSpec};

pub struct BuiltTask {
    pub name: String,
    pub entry: OwnedAddress,
    pub initial_stack_pointer: OwnedAddress,
    pub owned_regions: BTreeMap<String, OwnedRegion>,
}

pub struct OwnedRegion {
    pub base: u64,
    pub sizes: Vec<u64>,
}

impl OwnedRegion {
    pub fn contains(&self, addr: u64) -> bool {
        let size = self.sizes.iter().sum::<u64>();
        addr >= self.base && addr < (self.base + size)
    }
}

pub struct OwnedAddress {
    pub region: String,
    pub offset: u64,
}


/// Performs a second link pass on a partially linked executable.
///
/// `env`, `app`, `target`, and `targetroot` describe the overall build
/// configuration.
///
/// `def` specifies the task in the `app`. This should be a reference to one of
/// the `app.tasks`.
///
/// `inpath` gives the location of the partially linked executable, while
/// `outpath` will be written with the result.
///
/// `linker_script` gives the location of the linker script to use in this pass.
/// The script should contain a directive to include `memory.x`.
///
/// `alloc` is the results of memory allocation for the task, after the initial
/// build and analysis phase. It is a map from memory name (as given in the
/// chipdef) to allocation requirements.
///
/// On success, returns:
/// - A map from memory name to regions owned by this task.
/// - The task's initial stack pointer, in terms of those regions.
#[allow(clippy::too_many_arguments)]
fn relink(
    env: &BuildEnv,
    app: &AppDef,
    target: &TargetSpec,
    targetroot: &Path,
    def: &TaskDef,
    inpath: &Path,
    outpath: &Path,
    linker_script: &Path,
    alloc: &BTreeMap<String, TaskAllocation>,
) -> miette::Result<(BTreeMap<String, OwnedRegion>, Option<OwnedAddress>)> {
    // Generate the linker script "memory fragment" in the targetroot. This file
    // has to be called "memory.x" because it's pulled in by an include
    // directive under that name by the reusable part of the linker script.
    //
    // This fragment consists of
    // - The MEMORY section
    // - A SECTIONS add-on stanza containing any additional section mappings
    //   required for the task.
    let memory_frag_path = targetroot.join("memory.x");
    let mut scr = std::fs::File::create(&memory_frag_path)
        .into_diagnostic()?;
    writeln!(scr, "MEMORY {{").into_diagnostic()?;

    // During this same pass over the task memory map, we're going to determine
    // the value of the initial stack pointer and the set of owned memory
    // regions.
    let mut initial_stack_pointer = None;
    let mut owned_regions = BTreeMap::new();

    // For each memory type defined in the chipdef, check if the task has
    // any requirements in that memory.
    for orig_name in app.board.chip.memory.keys() {
        let Some(regalloc) = alloc.get(orig_name) else {
            // Nope!
            continue;
        };

        owned_regions.insert(orig_name.to_string(), OwnedRegion {
            base: regalloc.base,
            sizes: regalloc.sizes.clone(),
        });

        // Linker memory names are conventionally UPPERCASE, so:
        let name = orig_name.to_ascii_uppercase();

        let mut base = regalloc.base;
        let mut size = regalloc.calculate_size();

        if name == "RAM" {
            // We recognize this magical memory name as the place where stacks
            // go. Stacks are inserted at the bottom of the task's RAM area.
            //
            // TODO: the fact that this uses the memory name is kind of grody.

            // We generate a "memory" named STACK to describe this.
            writeln!(scr, "STACK (rw): ORIGIN = {base:#x}, LENGTH = {:#x}", def.stack_size.value()).into_diagnostic()?;
            // Deduct the stack from the overall RAM size.
            base += *def.stack_size.value();
            size -= *def.stack_size.value();
            // The stack pointer will start at the top of that deduction.
            initial_stack_pointer = Some(OwnedAddress {
                region: orig_name.to_string(),
                offset: *def.stack_size.value(),
            });
        }
        writeln!(scr, "{name} (rw): ORIGIN = {base:#x}, LENGTH = {size:#x}").into_diagnostic()?;
    }
    writeln!(scr, "}}").into_diagnostic()?;

    // And now, the SECTIONS section, to insert sections with your sections so
    // you can section your sections. ...
    //
    // Anyway, this handles any explicitly declared additional memory sections.
    writeln!(scr, "SECTIONS {{").into_diagnostic()?;
    for (section, memory) in &def.sections {
        // We tolerate section names not starting with a dot in the config, but
        // for consistency, add a dot in the linker script.
        let leader = if section.starts_with('.') { "" } else { "." };

        let memory = memory.to_ascii_uppercase();
        writeln!(scr, "{leader}{section} (NOLOAD) : ALIGN(4) {{").into_diagnostic()?;
        writeln!(scr, "  *({leader}{section} {leader}{section}.*);").into_diagnostic()?;
        writeln!(scr, "}} > {memory}").into_diagnostic()?;
    }
    writeln!(scr, "  ").into_diagnostic()?;
    // .uninit is our final useful section in the linker script, and generally
    // where things like this want to go.
    writeln!(scr, "}} INSERT AFTER .uninit").into_diagnostic()?;

    // All done!
    drop(scr);

    // Run the linker on the provided linker script, plus our fragment.
    let mut ldcmd = cmd_with_clean_env(&env.linker_path);
    ldcmd.arg(inpath);
    ldcmd.arg("-o").arg(outpath);
    ldcmd.arg(format!("-T{}", linker_script.display()));
    ldcmd.arg("--gc-sections");
    ldcmd.args(["-m", &target.bfd_name]);

    // TODO: are these still necessary?
    ldcmd.args(["-z", "common-page-size=0x20"]);
    ldcmd.args(["-z", "max-page-size=0x20"]);

    ldcmd.current_dir(targetroot);

    let ldstatus = ldcmd.status().into_diagnostic()?;
    if !ldstatus.success() {
        return Err(miette!("command failed"));
    }

    // Delete the memory fragment file, to avoid surprises.
    std::fs::remove_file(memory_frag_path).into_diagnostic()?;

    Ok((owned_regions, initial_stack_pointer))
}

/// Re-links a partially linked executable against a bogus memory map, to
/// determine its resource requirements.
///
/// Specifically, this links a partially-linked task as though it had sole
/// control of all of the memory in the chipdef. We then analyze the resulting
/// ELF executable to determine memory requirements in each memory region, and
/// return that.
///
/// `env`, `app`, `target`, and `targetroot` define the build environment.
///
/// `def` defines the task, and should be a reference to one of `app.tasks`.
///
/// `inpath` gives the path to the partially-linked ELF executable, and
/// `outpath` gives a location where we can write a linked-but-bogus equivalent.
///
/// `linker_script` is a path to the linker script to use at this stage.
#[allow(clippy::too_many_arguments)]
pub fn relink_for_size(
    env: &BuildEnv,
    app: &AppDef,
    target: &TargetSpec,
    targetroot: &Path,
    def: &TaskDef,
    inpath: &Path,
    outpath: &Path,
    linker_script: &Path,
) -> miette::Result<BTreeMap<String, RangeSet<u64>>> {
    // Make an "allocation result" that assigns this task all of memory.
    let alloc_everything = app.board.chip.memory.iter()
        .map(|(name, regdef)| (name.clone(), TaskAllocation {
            requested: 0,
            base: *regdef.value().base.value(),
            sizes: vec![*regdef.value().size.value()],
        }))
    .collect();
    // Relink the task to that allocation.
    relink(env, app, target, targetroot, def, inpath, outpath, linker_script, &alloc_everything)?;

    // Load the result and analyze the memory layout.
    let file_image = std::fs::read(outpath).into_diagnostic()?;

    collect_sizes(app, &file_image)
}

pub fn collect_sizes(
    app: &AppDef,
    file_image: &[u8],
) -> miette::Result<BTreeMap<String, RangeSet<u64>>> {
    let elf = goblin::elf::Elf::parse(file_image).into_diagnostic()?;

    // Initialize the size map with an entry for each type of memory defined in
    // the chipdef, but with a zero-size range at its base. We will enlarge
    // these ranges as required.
    let mut region_sizes = app.board.chip.memory.iter()
        .map(|(name, _region)| (name.clone(), RangeSet::new()))
        .collect::<BTreeMap<_, _>>();

    for phdr in &elf.program_headers {
        // Skip metadata and whatnot.
        if phdr.p_type != PT_LOAD {
            continue;
        }
        // Figure out which region contains this phdr.
        let (regname, _reg) = app.board.chip.memory.iter()
            .find(|(_name, reg)| *reg.value().base.value() <= phdr.p_vaddr && (phdr.p_vaddr + phdr.p_memsz) <= (reg.value().base.value() + reg.value().size.value()))
            .expect("internal inconsistency");

        // Borrow its size range in our output table.
        let sz = region_sizes.get_mut(regname).expect("internal inconsistency");
        // Enlarge the range to contain all of this item.
        sz.insert(phdr.p_vaddr .. phdr.p_vaddr + phdr.p_memsz);

        if phdr.p_vaddr != phdr.p_paddr {
            // This is an "initialized data" phdr, like the .data section. This
            // means it appears twice, once in RAM at its full memsz, and once
            // somewhere else (generally flash) with a possibly smaller filsz.
            //
            // Repeat the process, but use the paddr and filsz.
            let (regname, _reg) = app.board.chip.memory.iter()
                .find(|(_name, reg)| *reg.value().base.value() <= phdr.p_paddr && (phdr.p_paddr + phdr.p_filesz) <= (reg.value().base.value() + reg.value().size.value()))
                .expect("internal inconsistency");

            let sz = region_sizes.get_mut(regname).expect("internal inconsistency");
            sz.insert(phdr.p_paddr .. phdr.p_paddr + phdr.p_filesz);
        }
    }
    Ok(region_sizes)
}

pub fn merge_ranges(set: &RangeSet<u64>) -> Option<Range<u64>> {
    let start = set.first()?;
    let end = set.last()?;
    Some(start.start .. end.end)
}

/// Relink a partially-linked task executable in its final location.
///
/// `env`, `app`, `target`, and `targetroot` define the build environment.
///
/// `def` defines the task, and should be a reference to one of `app.tasks`.
///
/// `inpath` gives the path to the partially-linked ELF executable, and
/// `outpath` is where the final linked result will be placed.
///
/// `linker_script` is the path to the linker script we'll use for the final
/// link.
///
/// `alloc` is the set of allocation decisions for this task, defining its final
/// placement.
/// 
/// On success, returns a `BuiltTask` summarizing the output.
#[allow(clippy::too_many_arguments)]
pub fn relink_final(
    env: &BuildEnv,
    app: &AppDef,
    target: &TargetSpec,
    targetroot: &Path,
    def: &TaskDef,
    inpath: &Path,
    outpath: &Path,
    linker_script: &Path,
    alloc: &BTreeMap<String, TaskAllocation>,
) -> miette::Result<BuiltTask> {
    // Use the common relink routine to do most of the work.
    let (owned_regions, initial_stack_pointer) = relink(env, app, target, targetroot, def, inpath, outpath, linker_script, alloc)?;

    // There's one specific piece of information that we don't have yet: the
    // binary's entry address. While most Hubris binaries build themselves to
    // have their entry point at the base of flash, we don't actually _require_
    // this.
    //
    // Instead, we're going to parse the ELF headers.
    let file_image = std::fs::read(outpath).into_diagnostic()?;
    let elf = goblin::elf::Elf::parse(&file_image).into_diagnostic()?;

    let entry_region = owned_regions.iter()
        .find(|(_name, reg)| reg.contains(elf.entry));
    let entry_region = entry_region.expect("invalid entry point");

    let entry = OwnedAddress {
        region: entry_region.0.to_string(),
        offset: elf.entry - entry_region.1.base,
    };
    let initial_stack_pointer = initial_stack_pointer.expect("missing RAM region?");

    Ok(BuiltTask {
        name: def.name.clone(),
        entry,
        initial_stack_pointer,
        owned_regions,
    })
}

