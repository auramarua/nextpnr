#![feature(c_unwind)]
#![feature(let_chains)]

// use core::ffi::{c_char, c_int};
use std::collections::HashMap;
use std::pin::Pin;
use std::{ptr::NonNull, time::Instant};

use colored::Colorize;
use indicatif::MultiProgress;
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};

use crate::partition::Coord;

#[macro_use]
mod npnr;
mod partition;
mod route;

#[no_mangle]
pub extern "C-unwind" fn nextpnr_router_awooter(
    ctx: Option<NonNull<npnr::Context>>,
    pressure: f32,
    history: f32,
) -> bool {
    // One-time pre-execution router setup in Rust happens... now.
    // Notably, this means constructing Nets once, rather than giving it a callable constructor,
    // as it is almost certainly erroneous to construct it twice.
    // It's fine to query C++ for transient data after this, but if moving ownership from C++ to Rust,
    // or anything else that looks like "mutable borrow for the rest of the program", then do it now.

    let mut ctx = ctx.expect("Context* should be non-null");
    let mut dict = unsafe { npnr::npnr_context_nets(ctx.as_ptr()) };
    let mut index_to_net = Vec::new();
    let name_sz = unsafe { npnr::npnr_nets_names(&*dict, &mut index_to_net) };
    let mut nets = HashMap::new();
    let mut users = HashMap::new();
    for (i, &name) in index_to_net.iter().enumerate() {
        // We don't violate anything here.
        let mut net = unsafe {
            Pin::new_unchecked(dict.as_mut().expect("context should be non-null")).move_net(&name)
        };
        unsafe {
            npnr::npnr_netinfo_udata_set(net.pin_mut(), i as _);
        }
        // Leaking memory is the most convenient FFI I could think of.
        let mut net_users = Vec::new();
        unsafe { npnr::npnr_netinfo_users_leak(net.pin_mut(), &mut net_users) };

        // Hijinx!
        let liberated_ptr = std::mem::replace(net, cxx::UniquePtr::null());

        nets.insert(name, liberated_ptr);
        users.insert(name, net_users);
    }

    let nets = npnr::Nets {
        nets,
        users,
        index_to_net,
    };
    let ctx: Pin<&mut npnr::Context> = unsafe { Pin::new_unchecked(ctx.as_mut()) };
    route(ctx, nets, pressure, history)

    /*std::panic::catch_unwind(move || {
        let ctx: &mut npnr::Context = unsafe { ctx.expect("non-null context").as_mut() };
        route(ctx)
    })
    .unwrap_or_else(|x| {
        if let Ok(x) = x.downcast::<String>() {
            log_error!("caught panic: {}", x);
        }
        false
    })*/
}

fn extract_arcs_from_nets(ctx: &npnr::Context, nets: &npnr::Nets) -> Vec<route::Arc> {
    let mut arcs = vec![];
    for (&name, net) in &nets.nets {
        let s = ctx.name_of(name);
        let verbose = false; //str == "soc0.processor.with_fpu.fpu_0.fpu_multiply_0.rin_CCU2C_S0_4$CCU2_FCI_INT";

        if verbose {
            dbg!(s, net.is_global());
        }

        if net.is_global() {
            continue;
        }
        let port_ref = net.driver();
        let port_ref = unsafe { port_ref.as_ref().unwrap() };
        if let Some(cell) = port_ref.cell() {
            let source = cell.location();
            let source_wire = unsafe { ctx.source_wire(&**net) };

            for sink_ref in nets.users_by_name(name).unwrap().iter() {
                let sink = sink_ref.cell().unwrap();
                let sink = sink.location();
                for sink_wire in ctx.sink_wires(&net, sink_ref) {
                    arcs.push(route::Arc::new(
                        source_wire,
                        source,
                        sink_wire,
                        sink,
                        net.index(),
                        nets.name_from_index(net.index()),
                    ));

                    if verbose {
                        let source_wire = ctx.name_of_wire(source_wire);
                        let sink_wire = ctx.name_of_wire(sink_wire);
                        dbg!(source_wire, sink_wire, net.index().into_inner());
                    }
                }
            }
        }
    }
    arcs
}

fn route(mut ctx: Pin<&mut npnr::Context>, nets: npnr::Nets, pressure: f32, history: f32) -> bool {
    log_info!(
        "{}{}{}{}{}{} from Rust!\n",
        "A".red(),
        "w".green(),
        "o".yellow(),
        "o".blue(),
        "o".magenta(),
        "o".cyan()
    );
    log_info!(
        "Running on a {}x{} grid\n",
        ctx.grid_dim_x().to_string().bold(),
        ctx.grid_dim_y().to_string().bold(),
    );

    let wires = ctx.wires_leaking();
    log_info!("Found {} wires\n", wires.len().to_string().bold());

    let pips = ctx.pips_leaking();
    log_info!("Found {} pips\n", pips.len().to_string().bold());

    let nets_str = nets.len().to_string();
    log_info!("Found {} nets\n", nets_str.bold());

    let mut count = 0;
    for (&name, net) in &nets.nets {
        let users = nets.users_by_name(name).unwrap().iter();
        for user in users {
            count += ctx.sink_wires(&net, user).len();
        }
    }

    log_info!("Found {} arcs\n", count.to_string().bold());

    let binding = &nets.nets;
    let (name, net) = binding
        .into_iter()
        .max_by_key(|(name, net)| {
            if net.is_global() {
                0
            } else {
                nets.users_by_name(**name)
                    .unwrap()
                    .iter()
                    .fold(0, |acc, sink| acc + ctx.sink_wires(&net, sink).len())
            }
        })
        .unwrap();

    let count = nets
        .users_by_name(*name)
        .unwrap()
        .iter()
        .fold(0, |acc, sink| acc + ctx.sink_wires(&net, sink).len())
        .to_string();

    log_info!(
        "Highest non-global fanout net is {}\n",
        String::try_from(ctx.name_of(*name)).unwrap().bold()
    );
    log_info!("  with {} arcs\n", count.bold());

    let mut x0 = 0;
    let mut y0 = 0;
    let mut x1 = 0;
    let mut y1 = 0;

    for sink in nets.users_by_name(*name).unwrap().iter() {
        let cell = sink.cell().unwrap().location();
        x0 = x0.min(cell.x);
        y0 = y0.min(cell.y);
        x1 = x1.max(cell.x);
        y1 = y1.max(cell.y);
    }

    let coords_min = format!("({}, {})", x0, y0);
    let coords_max = format!("({}, {})", x1, y1);
    log_info!(
        "  which spans from {} to {}\n",
        coords_min.bold(),
        coords_max.bold()
    );

    log_info!(
        "rayon reports {} threads available\n",
        rayon::current_num_threads().to_string().bold()
    );

    let start = Instant::now();

    let arcs = extract_arcs_from_nets(&ctx, &nets);

    let mut special_arcs = vec![];
    let mut partitionable_arcs = Vec::with_capacity(arcs.len());
    for arc in arcs {
        let src_name = ctx.name_of_wire(arc.source_wire());
        let dst_name = ctx.name_of_wire(arc.sink_wire());

        if src_name.contains("FCO_SLICE")
            || src_name.contains("Q6_SLICE")
            || src_name.contains('J')
            || src_name.contains("DDR")
            || dst_name.contains("DDR")
            || dst_name.contains("X126/Y20/PADDOD_PIO")
        {
            special_arcs.push(arc);
        } else {
            partitionable_arcs.push(arc);
        }
    }
    log_info!(
        "  {} arcs special-cased\n",
        special_arcs.len().to_string().bold()
    );

    let mut partitions = vec![(
        Coord::new(0, 0),
        Coord::new(ctx.grid_dim_x(), ctx.grid_dim_y()),
        partitionable_arcs,
        String::from(""),
    )];

    for _ in 0..2 {
        let mut new_partitions = Vec::with_capacity(partitions.len() * 4);
        for (min, max, partition, name) in &partitions {
            log_info!("partition {}:\n", name);
            let (x_part, y_part, ne, se, sw, nw, special) =
                partition::find_partition_point_and_sanity_check(
                    &mut ctx, &nets, partition, &pips, min.x, max.x, min.y, max.y,
                );
            special_arcs.extend(special.into_iter());
            new_partitions.push((
                Coord::new(x_part, min.y),
                Coord::new(max.x, y_part),
                se,
                format!("{}_SE", name),
            ));
            new_partitions.push((Coord::new(x_part, y_part), *max, sw, format!("{}_SW", name)));
            new_partitions.push((*min, Coord::new(x_part, y_part), ne, format!("{}_NE", name)));
            new_partitions.push((
                Coord::new(min.x, y_part),
                Coord::new(x_part, max.y),
                nw,
                format!("{}_NW", name),
            ));
        }
        partitions = new_partitions;
    }

    let time = format!("{:.2}", (Instant::now() - start).as_secs_f32());
    log_info!("Partitioning took {}s\n", time.bold());

    log_info!(
        "now {} arcs special-cased\n",
        special_arcs.len().to_string().bold()
    );

    log_info!(
        "Using pressure factor {} and history factor {}\n",
        pressure,
        history
    );

    let start = Instant::now();

    log_info!("Routing partitioned arcs\n");

    let progress = MultiProgress::new();

    let router = route::Router::new(&nets, &wires, pressure, history);
    partitions
        .par_iter()
        .for_each(|(box_ne, box_sw, arcs, id)| {
            let mut thread = route::RouterThread::new(*box_ne, *box_sw, arcs, id, &progress);
            router.route(&ctx, &nets, &mut thread);
        });

    log_info!("Routing miscellaneous arcs\n");
    let mut thread = route::RouterThread::new(
        Coord::new(0, 0),
        Coord::new(ctx.grid_dim_x(), ctx.grid_dim_y()),
        &special_arcs,
        "MISC",
        &progress,
    );

    router.route(&ctx, &nets, &mut thread);

    let time = format!("{:.2}", (Instant::now() - start).as_secs_f32());
    log_info!("Routing took {}s\n", time.bold());

    //let mut router = route::Router::new(Coord::new(0, 0), Coord::new(x_part, y_part));

    /*log_info!("=== level 2 NE:\n");
    let _ = find_partition_point(&ne, x_start, x, y_start, y);
    log_info!("=== level 2 SE:\n");
    let _ = find_partition_point(&se, x, x_finish, y_start, y);
    log_info!("=== level 2 SW:\n");
    let _ = find_partition_point(&sw, x, x_finish, y, y_finish);
    log_info!("=== level 2 NW:\n");
    let _ = find_partition_point(&nw, x_start, x, y, y_finish);*/

    true
}
