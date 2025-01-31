use std::collections::BTreeMap;

use comfy_table::{Attribute, Cell, CellAlignment};
use indexmap::IndexMap;
use rangemap::RangeSet;
use size::Size;

use crate::{alloc::TaskAllocation, appcfg::AppDef};

pub fn simple_table<'a>(content: impl IntoIterator<Item = (&'a str, &'a str)>) {
    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
    table.apply_modifier(comfy_table::modifiers::UTF8_SOLID_INNER_BORDERS);
    table.set_content_arrangement(comfy_table::ContentArrangement::Dynamic);

    for row in content {
        table.add_row([row.0, row.1]);
    }

    println!();
    println!();
    println!("{table}");
    println!();
}

pub fn banner(content: impl core::fmt::Display) {
    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::UTF8_FULL);
    table.apply_modifier(comfy_table::modifiers::UTF8_ROUND_CORNERS);
    table.set_content_arrangement(comfy_table::ContentArrangement::Dynamic);

    table.add_row([content]);

    println!();
    println!();
    println!("{table}");
    println!();
}

pub fn print_allocations(
    app: &AppDef,
    task_allocs: &BTreeMap<&str, IndexMap<&str, &TaskAllocation>>,
    kernel_allocs: &BTreeMap<String, RangeSet<u64>>,
) {
    let mut table = comfy_table::Table::new();
    table.load_preset(comfy_table::presets::NOTHING);
    table.set_header(["MEMORY", "OWNER", "START", "END", "SIZE", "WASTE"]);
    table.column_mut(2).unwrap().set_cell_alignment(CellAlignment::Right);
    table.column_mut(3).unwrap().set_cell_alignment(CellAlignment::Right);
    table.column_mut(4).unwrap().set_cell_alignment(CellAlignment::Right);
    table.column_mut(5).unwrap().set_cell_alignment(CellAlignment::Right);
    for region_name in app.board.chip.memory.keys() {
        let mut regrows = vec![];
        let mut total = 0;
        let mut loss = 0;
        let mut last = None;

        if let Some(regallocs) = task_allocs.get(region_name.as_str()) {
            let mut regallocs = regallocs.iter().collect::<Vec<_>>();
            regallocs.sort_by_key(|(_name, ta)| ta.base);

            for (task_name, talloc) in regallocs {
                let base = talloc.base;
                let req_size = talloc.requested;

                if let Some(last) = last {
                    if base != last {
                        let pad_size = base - last;
                        total += pad_size;
                        loss += pad_size;
                        regrows.push([
                            Cell::new(region_name).dim(),
                            Cell::new("-pad-").dim(),
                            Cell::new(format!("{:#x}", last)).dim(),
                            Cell::new(format!("{:#x}", base - 1)).dim(),
                            Cell::new(Size::from_bytes(pad_size)).dim(),
                            Cell::new(Size::from_bytes(pad_size)).dim(),
                        ]);
                    }
                }
                let size = talloc.sizes.iter().sum::<u64>();
                let internal_pad = size - req_size;
                regrows.push([
                    Cell::new(region_name),
                    Cell::new(task_name),
                    Cell::new(format!("{:#x}", base)),
                    Cell::new(format!("{:#x}", base + size - 1)),
                    Cell::new(Size::from_bytes(size)),
                    Cell::new(Size::from_bytes(internal_pad)),
                ]);
                total += size;
                loss += internal_pad;

                last = Some(base + size);
            }
        }
        if let Some(kallocs) = kernel_allocs.get(region_name) {
            for kalloc in kallocs.iter() {
                let size = kalloc.end - kalloc.start;
                regrows.push([
                    Cell::new(region_name),
                    Cell::new("kernel"),
                    Cell::new(format!("{:#x}", kalloc.start)),
                    Cell::new(format!("{:#x}", kalloc.end - 1)),
                    Cell::new(Size::from_bytes(size)),
                    Cell::new(Size::from_bytes(0)),
                ]);
                total += size;
            }
        }

        regrows.sort_by(|a, b| a[2].content().cmp(&b[2].content()));

        if !regrows.is_empty() {
            table.add_rows(regrows);
            table.add_row([
                Cell::new(region_name).bold().underlined(),
                Cell::new("(total)").bold().underlined(),
                Cell::new("").underlined(),
                Cell::new("").underlined(),
                Cell::new(Size::from_bytes(total)).bold().underlined(),
                Cell::new(Size::from_bytes(loss)).bold().underlined(),
            ]);
        }
    }
    println!("{table}");
}

trait CellExt {
    fn dim(self) -> Self;
    fn bold(self) -> Self;
    fn underlined(self) -> Self;
}

impl CellExt for Cell {
    fn dim(self) -> Self {
        self.add_attribute(Attribute::Dim)
    }
    fn bold(self) -> Self {
        self.add_attribute(Attribute::Bold)
    }
    fn underlined(self) -> Self {
        self.add_attribute(Attribute::Underlined)
    }
}
