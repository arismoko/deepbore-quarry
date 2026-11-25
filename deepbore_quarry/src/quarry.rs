//! The quarry's block entity: its state, its tick, its menu and screen, and how clients draw it.

use crate::{keys, rules, upgrade_of, world};
use mod_api::{
    BlockEntityPart, BlockEntityRenderBoundingBoxEvent, BlockEntitySubmitEvent,
    BlockEntityTickEvent, BlockEntityUpdateTagEvent, BlockPart, BlockState, BlockUpdateFlags,
    ClientHost, Compound, CubeFace, Direction, DrawContext, Flow, Host, HostBlockStates,
    HostTransaction, ITEMS, ItemPart, ItemResource, ItemStack, LevelRef, LightLayer,
    MenuButtonEvent, MenuButtonValidEvent, MenuContainer, MenuPart, MenuScreen, MenuScreenFactory,
    MenuScreenFrame, MenuScreenLayout, MenuScreenOpen, MenuSlotRulesEvent, OnBlockPlacedByEvent,
    ParticleOptions, ScreenSlot, ServerHost, SoundSource, Tag, Text,
};
use rules::{Action, Cell, Input, Quarry as State, Upgrade, Upgrades};
use std::{collections::HashMap, sync::Mutex};

// ---- a quarry's state ----------------------------------------------------------

const KEY: &str = "quarry";

fn load(nbt: &Compound) -> Option<State> {
    match nbt.get(&KEY.into()) {
        Some(Tag::ByteArray(bytes)) => serde_json::from_slice(bytes).ok(),
        _ => None,
    }
}

/// A quarry's state as the saved field it is kept in.
fn encode(quarry: &State) -> Option<Tag> {
    serde_json::to_vec(quarry).ok().map(Tag::ByteArray)
}

fn dig_direction(host: &dyn ServerHost, level: LevelRef, pos: [i32; 3]) -> rules::Facing {
    level
        .block_property(host, pos, "facing")
        .and_then(|f| rules::Facing::away_from(f))
        .unwrap_or(rules::Facing::North)
}

fn upgrade_slots(items: &[Option<ItemStack>]) -> [Option<Upgrade>; rules::UPGRADE_SLOTS] {
    std::array::from_fn(|i| {
        items
            .get(rules::FIRST_UPGRADE + i)
            .and_then(Option::as_ref)
            .and_then(|s| upgrade_of(s.item()))
    })
}

/// Whether the acting player is kept from the quarry at `pos`, and told so: it is its owner's, or an
/// operator's of level 2.
pub(crate) fn refused(host: &mut dyn ServerHost, pos: [i32; 3]) -> bool {
    let Some(player) = host.players().acting_player() else {
        return false;
    };
    let Some(level) = player.level(host) else {
        return false;
    };
    if level.block(host, pos) != Some(keys().block) {
        return false;
    }
    let owner = host
        .block_entity(level, pos)
        .and_then(|be| be.data(host))
        .and_then(|data| load(&data))
        .map_or(0, |q| q.owner);
    let op = player.permission_level(host).unwrap_or(0) >= 2;
    if owner == 0 || owner == player.uuid() || op {
        return false;
    }
    let _ = player.tell(
        host,
        Text::translate("deepbore_quarry.not_owner", Vec::new()),
    );
    true
}

/// The quarry's item: a quarry placed from it is its placer's.
pub(crate) struct QuarryItem;

#[mod_api::sunrise_part]
impl ItemPart for QuarryItem {
    fn on_block_placed_by(&self, host: &mut dyn ServerHost, event: &mut OnBlockPlacedByEvent) {
        let Some(player) = event.placer else {
            return;
        };
        let Some(level) = player.level(host) else {
            return;
        };
        let quarry = State::new(
            player.uuid(),
            dig_direction(host, level, event.pos),
            event.pos,
        );
        let Some(be) = host.block_entity(level, event.pos) else {
            return;
        };
        let Some(tag) = encode(&quarry) else {
            return;
        };
        let mut data = Compound::default();
        data.insert(KEY.into(), tag);
        let _ = be.merge_data(host, data);
    }
}

// ---- the tick ------------------------------------------------------------------

/// The sides a chest may be on, and the chest's side that faces the quarry.
const AROUND: [([i32; 3], Direction); 6] = [
    ([0, -1, 0], Direction::Up),
    ([0, 1, 0], Direction::Down),
    ([0, 0, -1], Direction::South),
    ([0, 0, 1], Direction::North),
    ([-1, 0, 0], Direction::East),
    ([1, 0, 0], Direction::West),
];

fn offset(pos: [i32; 3], by: [i32; 3]) -> [i32; 3] {
    [pos[0] + by[0], pos[1] + by[1], pos[2] + by[2]]
}

/// A chest, barrel or shulker box beside the quarry with room for items.
fn find_chest(
    host: &mut dyn ServerHost,
    level: LevelRef,
    pos: [i32; 3],
) -> Option<([i32; 3], Direction)> {
    AROUND.iter().find_map(|(by, face)| {
        let at = offset(pos, *by);
        let storage = level
            .block(host, at)
            .is_some_and(|b| rules::is_storage(b.id(host).unwrap_or_default()));
        (storage
            && level
                .capability(host, &ITEMS, at, Some(*face))
                .is_some_and(|items| items.size(host) > 0))
        .then_some((at, *face))
    })
}

/// Puts `stack` into the container at `at` from `face`; what doesn't fit comes back.
fn insert(
    host: &mut dyn ServerHost,
    level: LevelRef,
    (at, face): ([i32; 3], Direction),
    stack: ItemStack,
) -> Option<ItemStack> {
    let Some(items) = level.capability(host, &ITEMS, at, Some(face)) else {
        return Some(stack);
    };
    let resource = ItemResource::of(&stack);
    let count = stack.count();
    let moved = host
        .transaction(|host, tx| {
            Ok::<u32, ()>(items.insert(host, tx, &resource, count).unwrap_or(0))
        })
        .unwrap_or(0);
    resource.to_stack(count - moved)
}

/// Sends what waits in the hidden slots to the chest, as far as it has room.
fn flush(
    host: &mut dyn ServerHost,
    level: LevelRef,
    items: &mut [Option<ItemStack>],
    chest: ([i32; 3], Direction),
) {
    for slot in &mut items[rules::FIRST_BUFFER..rules::SLOTS] {
        if let Some(stack) = slot.clone() {
            let rest = insert(host, level, chest, stack.clone());
            if rest.as_ref() != Some(&stack) {
                *slot = rest;
            }
        }
    }
}

fn keep(items: &mut [Option<ItemStack>], stack: ItemStack) {
    match (rules::FIRST_BUFFER..rules::SLOTS).find(|s| items[*s].is_none()) {
        Some(slot) => items[slot] = Some(stack),
        // One block never drops more kinds than there are slots.
        None => log::warn!("[deepbore_quarry] no room for a dug item"),
    }
}

fn burn_time(host: &dyn Host, stack: &ItemStack) -> i32 {
    host.item_burn_time(stack.item()).unwrap_or(0)
}

fn classify(host: &mut dyn ServerHost, level: LevelRef, at: [i32; 3]) -> Cell {
    if !level.is_loaded(host, at) {
        return Cell::Unloaded;
    }
    let Some(state) = level.block_state(host, at) else {
        return Cell::Unloaded;
    };
    let block = state.block();
    if keys().airs.contains(&block) {
        return Cell::Air;
    }
    let fluid = level.fluid_state(host, at).is_some_and(|f| f.amount > 0);
    if fluid && level.block_property(host, at, "waterlogged") != Some("true") {
        return Cell::Fluid;
    }
    if host
        .block_states()
        .destroy_speed(state)
        .is_some_and(|h| h < 0.0)
    {
        return Cell::Unbreakable;
    }
    let contents = host.block_states().has_block_entity(state).unwrap_or(false);
    if block == keys().block || (contents && block.id(host) != Some("minecraft:spawner")) {
        return Cell::Kept;
    }
    Cell::Solid
}

/// Digs the block at `at` as an unenchanted diamond pickaxe would, which harvests everything breakable a
/// pickaxe can, obsidian too. A fluid is plugged with cobblestone, so it can't run into the pit.
fn dig(
    host: &mut dyn ServerHost,
    level: LevelRef,
    items: &mut [Option<ItemStack>],
    at: [i32; 3],
    fluid: bool,
    chest: Option<([i32; 3], Direction)>,
) {
    let Some(state) = level.block_state(host, at) else {
        return;
    };
    let loot = if fluid {
        Vec::new()
    } else {
        let tool = ItemStack::of(keys().pickaxe);
        level
            .block_drops(host, at, Some(&tool), None)
            .ok()
            .flatten()
            .unwrap_or_default()
    };
    let place = if fluid {
        keys().cobblestone
    } else {
        keys().airs[0]
    };
    let Some(new) = host.block_state_default(place) else {
        return;
    };
    let _ = level.set_block(
        host,
        at,
        new,
        BlockUpdateFlags::NEIGHBORS | BlockUpdateFlags::CLIENTS,
    );
    for stack in loot {
        let rest = match chest {
            Some(chest) => insert(host, level, chest, stack),
            None => Some(stack),
        };
        if let Some(rest) = rest {
            keep(items, rest);
        }
    }
    effects(host, level, at, state);
}

fn effects(host: &mut dyn ServerHost, level: LevelRef, at: [i32; 3], state: BlockState) {
    let center = at.map(|c| f64::from(c) + 0.5);
    let particle = ParticleOptions::simple(keys().block_particle);
    level.send_particles(host, &particle, Some(state), center, [0.25; 3], 0.1, 8);
    level.play_sound(
        host,
        "minecraft:block.stone.break",
        center,
        SoundSource::Blocks,
        0.6,
        1.0,
        None,
    );
}

/// What can't burn leaves the fuel slot for the chest: lava's empty bucket, and whatever a hopper or a
/// click put there, which would block the fuel behind it.
fn clear_fuel_slot(host: &dyn Host, items: &mut [Option<ItemStack>]) {
    let useless = items[rules::FUEL_SLOT]
        .as_ref()
        .is_some_and(|s| burn_time(host, s) == 0);
    let room = items[rules::FIRST_BUFFER..].iter().any(Option::is_none);
    if useless
        && room
        && let Some(stack) = items[rules::FUEL_SLOT].take()
    {
        keep(items, stack);
    }
}

fn take_fuel(items: &mut [Option<ItemStack>]) {
    let Some(stack) = items[rules::FUEL_SLOT].as_mut() else {
        return;
    };
    let lava = stack.item() == keys().lava_bucket;
    let left = stack.count().saturating_sub(1);
    if left == 0 {
        items[rules::FUEL_SLOT] = lava.then(|| ItemStack::of(keys().bucket));
    } else {
        let _ = stack.set_count(left);
    }
}

/// Holds or lets go of the quarry's chunks; true when another quarry of its owner holds them instead.
fn hold_chunks(
    host: &dyn ServerHost,
    quarry: &State,
    upgrades: Upgrades,
    dimension: &str,
    pos: [i32; 3],
    chest: Option<([i32; 3], Direction)>,
) -> bool {
    let Some(mut guard) = world::world(host) else {
        return false;
    };
    let st = guard.as_mut().expect("opened");
    let key = (dimension.to_owned(), pos);
    if !(quarry.on && upgrades.chunk && quarry.cursor.is_some()) {
        if st.loads.holds(&key) {
            let change = st.loads.release(&key);
            world::ticket(&mut st.tickets, &key, change);
            st.loads_changed = true;
        }
        return false;
    }
    let chunks = quarry.area(pos).chunks(pos, chest.map(|c| c.0));
    match st.loads.hold(quarry.owner, &key, chunks) {
        Some(change) => {
            if !change.add.is_empty() || !change.remove.is_empty() {
                world::ticket(&mut st.tickets, &key, change);
                st.loads_changed = true;
            }
            false
        }
        None => true,
    }
}

/// The quarry's tick: its slots, saved state and menu data are read from the ticking entity, the world is
/// worked through the host, and what changed is written back.
fn tick(event: &mut BlockEntityTickEvent<'_>) {
    let scope = &mut event.subject;
    let Some((level, pos, items, was, data, holder)) = scope.with_view(|view| {
        let items: Vec<Option<ItemStack>> = (0..rules::SLOTS as u32)
            .map(|i| view.item_slot(i).and_then(|s| s.to_owned()))
            .collect();
        (
            view.level(),
            view.position(),
            items,
            load(view.nbt()),
            view.data_slots().to_vec(),
            view.handle(),
        )
    }) else {
        return;
    };
    let mut entity = Entity {
        items: items.clone(),
        data,
    };
    let Some((quarry, sync)) = work(scope.host(), level, pos, &mut entity, was.clone()) else {
        return;
    };
    scope.with_view(|mut view| {
        for (slot, (old, new)) in items.iter().zip(&entity.items).enumerate() {
            if old != new {
                let _ = view.set_item_slot(slot as u32, new.clone());
            }
        }
        if view.data_slots() != entity.data.as_slice() {
            view.set_data_slots(&entity.data);
        }
        if was.as_ref() != Some(&quarry)
            && let Some(tag) = encode(&quarry)
        {
            view.set_nbt(KEY.into(), Some(tag));
        }
    });
    if sync {
        let _ = holder.sync(scope.host());
    }
}

/// The ticking entity's slots and menu data, as the tick works on them.
struct Entity {
    items: Vec<Option<ItemStack>>,
    data: Vec<i32>,
}

/// One tick of work: the quarry's new state, and whether clients should hear of it.
fn work(
    host: &mut dyn ServerHost,
    level: LevelRef,
    pos: [i32; 3],
    entity: &mut Entity,
    was: Option<State>,
) -> Option<(State, bool)> {
    let dimension = level.dimension().id(host)?.to_owned();
    // Placed some other way than from its item: nobody's.
    let mut quarry = was
        .clone()
        .unwrap_or_else(|| State::new(0, dig_direction(host, level, pos), pos));
    let upgrades = Upgrades::from_slots(&upgrade_slots(&entity.items));
    quarry.resize(pos, rules::side(upgrades.area));

    let chest = find_chest(host, level, pos);
    if let Some(chest) = chest {
        clear_fuel_slot(host, &mut entity.items);
        flush(host, level, &mut entity.items, chest);
    }
    let pending = entity.items[rules::FIRST_BUFFER..]
        .iter()
        .any(Option::is_some);
    let powered = level.has_neighbor_signal(host, pos);
    let min_y = level.height(host).map_or(-64, |(min_y, _)| min_y);
    let max_side = world::max_side(host);

    let mut action = Action::Wait;
    let mut broke = false;
    for _ in 0..rules::SKIPS_PER_TICK {
        let cell = quarry
            .target(pos)
            .map_or(Cell::Air, |at| classify(host, level, at));
        let fuel = entity.items[rules::FUEL_SLOT]
            .as_ref()
            .map(|s| burn_time(host, s))
            .filter(|t| *t > 0);
        let input = Input {
            upgrades,
            powered,
            max_side,
            min_y,
            chest: chest.is_some(),
            pending,
            fuel,
            cell,
        };
        action = rules::step(&mut quarry, pos, &input);
        match action {
            Action::Skip => {}
            Action::Burn => {
                quarry.burn += fuel.unwrap_or(0);
                take_fuel(&mut entity.items);
            }
            Action::Break { pos: at, fluid } => {
                dig(host, level, &mut entity.items, at, fluid, chest);
                broke = true;
                break;
            }
            Action::Stopped(_) | Action::Wait => break,
        }
    }

    let refused = hold_chunks(host, &quarry, upgrades, &dimension, pos, chest);
    let status = rules::status_code(&action);
    let in_slot = entity.items[rules::FUEL_SLOT]
        .as_ref()
        .map_or(0, |s| i64::from(burn_time(host, s)) * i64::from(s.count()));
    let data = vec![
        status,
        ((i64::from(quarry.burn) + in_slot) / i64::from(rules::BURN_PER_BLOCK)).min(32767) as i32,
        quarry.cursor.map_or(i32::from(i16::MIN), |c| c.y),
        quarry.side,
        i32::from(quarry.on),
        i32::from(refused),
    ];
    // What clients draw changes with the target and with working or not.
    let working = |d: &[i32]| d.first() == Some(&0);
    let sync = broke
        || working(&data) != working(&entity.data)
        || was.as_ref().is_none_or(|w| w.side != quarry.side);
    entity.data = data;
    Some((quarry, sync))
}

// ---- the menu ------------------------------------------------------------------

const TOGGLE: i32 = 0;
/// `[x, y, width, height]`, from the screen's top left.
const BUTTON: [f32; 4] = [136.0, 34.0, 32.0, 18.0];

/// The menu's slots: fuel in the first, one upgrade in each of the next four (no more of a kind than it allows),
/// nothing in the hidden ones. The game's own shift-click follows these rules, so it moves items without the mod
/// counting them.
pub(crate) struct QuarryMenu;

#[mod_api::sunrise_part]
impl MenuPart for QuarryMenu {
    /// The start/stop button is the menu's one button.
    fn menu_button_valid(&self, _host: &dyn Host, event: &mut MenuButtonValidEvent<'_>) -> Flow {
        if event.id == TOGGLE {
            Flow::Cancel
        } else {
            Flow::Continue
        }
    }

    /// The start/stop button.
    fn menu_button(&self, event: &mut MenuButtonEvent<'_>) {
        if event.context.id != TOGGLE {
            return;
        }
        event.subject.with_view(|mut view| {
            if let Some(mut quarry) = load(view.nbt()) {
                quarry.on = !quarry.on;
                if let Some(tag) = encode(&quarry) {
                    view.set_nbt(KEY.into(), Some(tag));
                }
            }
        });
    }

    fn menu_slot_rules(&self, host: &dyn Host, event: &mut MenuSlotRulesEvent<'_>) {
        if event.container != MenuContainer::Main {
            return;
        }
        let upgrade = upgrade_of(event.stack.item());
        match event.slot {
            0 => event.may_place = upgrade.is_none() && burn_time(host, event.stack) > 0,
            1..5 => {
                let main: Vec<Option<ItemStack>> = (0..rules::SLOTS)
                    .map(|i| event.context.slot(MenuContainer::Main, i))
                    .collect();
                event.may_place = upgrade.is_some_and(|kind| {
                    rules::accepts(&upgrade_slots(&main), event.slot - 1, kind)
                });
                event.max_stack = 1;
            }
            _ => event.may_place = false,
        }
    }
}

// ---- the screen ----------------------------------------------------------------

/// A language line with its `%s` filled in, in the player's language.
fn tr(host: &dyn ClientHost, key: &str, args: &[String]) -> String {
    let text = Text::translate(key, args.iter().map(|a| Text::literal(a.clone())).collect());
    host.text_string(&text)
}

/// The quarry's screen: its slots where `tools/make_textures.py` draws their frames (the slots past this list are
/// hidden, and the server's menu leaves them out), its status lines and the start/stop button.
pub(crate) struct QuarryScreen;

impl MenuScreenFactory for QuarryScreen {
    fn create(&self, _: &dyn ClientHost, _: MenuScreenOpen<'_>) -> Option<MenuScreen> {
        let slot = |x: f32| {
            Some(ScreenSlot {
                position: [x, 35.0],
                no_item_icon: None,
            })
        };
        let layout = MenuScreenLayout {
            width: 176.0,
            height: 166.0,
            slots: vec![slot(17.0), slot(62.0), slot(80.0), slot(98.0), slot(116.0)],
            inventory_y: 84.0,
            inventory_x: 8.0,
            own_labels: true,
            own_background: true,
            ..Default::default()
        };
        Some(
            MenuScreen::new(layout)
                .with_draw(screen_draw)
                .with_click(|_, frame, right| {
                    let [bx, by, bw, bh] = BUTTON;
                    let [ax, ay] = frame.at;
                    (!right && (bx..bx + bw).contains(&ax) && (by..by + bh).contains(&ay))
                        .then_some(TOGGLE)
                }),
        )
    }
}

fn screen_draw(host: &dyn ClientHost, frame: MenuScreenFrame<'_>, draw: &mut dyn DrawContext) {
    let [x, y] = frame.origin;
    draw.sprite(
        "deepbore_quarry:gui/quarry",
        [0.0, 0.0, 176.0, 166.0],
        [x, y, 176.0, 166.0],
        None,
    );
    const LABEL: u32 = 0x40_4040;
    let get = |i: usize| frame.data.get(i).copied().unwrap_or(0);
    let mut text = |line: &str, at: [f32; 2], rgb: u32| {
        draw.text(line, [x + at[0], y + at[1]], 0xFF00_0000 | rgb, false)
    };
    text(
        &tr(host, "deepbore_quarry.screen.title", &[]),
        [8.0, 6.0],
        LABEL,
    );
    let status = tr(host, &format!("deepbore_quarry.status.{}", get(0)), &[]);
    text(
        &status,
        [8.0, 20.0],
        if get(0) == 0 { 0x20_7020 } else { 0xA0_3020 },
    );
    let side = get(3).to_string();
    text(
        &tr(host, "deepbore_quarry.screen.fuel", &[get(1).to_string()]),
        [8.0, 55.0],
        LABEL,
    );
    text(
        &tr(host, "deepbore_quarry.screen.size", &[side.clone(), side]),
        [100.0, 55.0],
        LABEL,
    );
    let layer = get(2) as i16;
    if layer != i16::MIN {
        text(
            &tr(host, "deepbore_quarry.screen.depth", &[layer.to_string()]),
            [8.0, 65.0],
            LABEL,
        );
    }
    if get(5) != 0 {
        text(
            &tr(host, "deepbore_quarry.chunk.taken", &[]),
            [8.0, 74.0],
            0xA0_3020,
        );
    }
    let [bx, by, bw, bh] = BUTTON;
    draw.fill([x + bx, y + by, bw, bh], 0xFF55_5555);
    draw.fill(
        [x + bx + 1.0, y + by + 1.0, bw - 2.0, bh - 2.0],
        0xFF8B_8B8B,
    );
    let label = tr(
        host,
        if get(4) != 0 {
            "deepbore_quarry.screen.stop"
        } else {
            "deepbore_quarry.screen.start"
        },
        &[],
    );
    let width = draw.text_width(&label);
    draw.text(
        &label,
        [x + bx + (bw - width) / 2.0, y + by + 5.0],
        0xFFFF_FFFF,
        false,
    );
}

// ---- the drawing ---------------------------------------------------------------

/// How far above the quarry the gantry runs.
const GANTRY: f32 = 3.0;
const FRAME: &str = "deepbore_quarry:block/frame";
const DRILL: &str = "deepbore_quarry:block/drill";

struct Head {
    at: [f32; 3],
    /// The frame it was last drawn: game time and partial tick.
    time: (i64, f32),
}

/// Where each quarry's head is, on this client, so it moves between two blocks.
static HEADS: Mutex<Option<HashMap<[i32; 3], Head>>> = Mutex::new(None);

/// What clients get of a quarry: the square, whether it works, its pace and its target.
fn update_tag(event: &mut BlockEntityUpdateTagEvent<'_>) -> Option<Compound> {
    let pos = event.pos;
    let quarry = load(event.entity.nbt())?;
    let area = quarry.area(pos);
    let upgrades = Upgrades::from_slots(&upgrade_slots(
        &(0..rules::SLOTS as u32)
            .map(|i| event.entity.copy_item_slot(i))
            .collect::<Vec<_>>(),
    ));
    let target = quarry.target(pos);
    let [tx, ty, tz] = target.unwrap_or(pos);
    let view = vec![
        area.min[0],
        area.min[1],
        area.max[0],
        area.max[1],
        i32::from(event.entity.data_slots().first() == Some(&0)),
        rules::ticks_per_block(upgrades.speed) as i32,
        i32::from(target.is_some()),
        tx,
        ty,
        tz,
    ];
    let mut tag = Compound::default();
    tag.insert("view".into(), Tag::IntArray(view));
    Some(tag)
}

fn submit(host: &dyn ClientHost, event: &mut BlockEntitySubmitEvent<'_>) {
    let pos = event.pos;
    let Some(Tag::IntArray(view)) = event.entity.nbt().get(&"view".into()) else {
        return;
    };
    let &[
        min_x,
        min_z,
        max_x,
        max_z,
        working,
        ticks,
        has_target,
        tx,
        ty,
        tz,
    ] = view.as_slice()
    else {
        return;
    };
    let draw = &mut *event.draw;
    // The quarry's own block is dark inside: the light is the air's above it.
    if let Some(level) = host.level(host.dimension()) {
        let above = [pos[0], pos[1] + 1, pos[2]];
        let block = level
            .brightness(host, LightLayer::Block, above)
            .unwrap_or(0)
            .max(0) as u32;
        let sky = level
            .brightness(host, LightLayer::Sky, above)
            .unwrap_or(0)
            .max(0) as u32;
        draw.set_light((sky * 16) | ((block * 16) << 8));
    }
    let mut part = |sprite: &str, at: [f32; 3], from: [f32; 3], to: [f32; 3]| {
        let face = Some(CubeFace::new(sprite));
        draw.push();
        draw.translate(at);
        draw.cube(from, to, [face, face, face, face, face, face], 0xFF_FFFF);
        draw.pop();
    };
    let (x0, z0) = ((min_x - pos[0]) as f32, (min_z - pos[2]) as f32);
    let (x1, z1) = ((max_x - pos[0]) as f32, (max_z - pos[2]) as f32);
    let along_x = ([0.0, 6.0, 6.0], [16.0, 10.0, 10.0]);
    let along_z = ([6.0, 6.0, 0.0], [10.0, 10.0, 16.0]);
    let upright = ([6.0, 0.0, 6.0], [10.0, 16.0, 10.0]);

    // The frame around the square, with a post at each corner.
    let mut x = x0;
    while x <= x1 {
        part(FRAME, [x, GANTRY, z0], along_x.0, along_x.1);
        part(FRAME, [x, GANTRY, z1], along_x.0, along_x.1);
        x += 1.0;
    }
    let mut z = z0;
    while z <= z1 {
        part(FRAME, [x0, GANTRY, z], along_z.0, along_z.1);
        part(FRAME, [x1, GANTRY, z], along_z.0, along_z.1);
        z += 1.0;
    }
    for corner in [[x0, z0], [x1, z0], [x0, z1], [x1, z1]] {
        let mut y = 0.0;
        while y < GANTRY {
            part(FRAME, [corner[0], y, corner[1]], upright.0, upright.1);
            y += 1.0;
        }
    }

    // The head: it moves on towards the block above the target, smoothly.
    let time = (event.draw.game_time(), event.draw.partial());
    let centre = [(x0 + x1) / 2.0, GANTRY, (z0 + z1) / 2.0];
    let target = if has_target != 0 {
        [
            (tx - pos[0]) as f32,
            (ty - pos[1]) as f32 + 1.0,
            (tz - pos[2]) as f32,
        ]
    } else {
        centre
    };
    let head = {
        let mut heads = HEADS.lock().unwrap_or_else(|e| e.into_inner());
        let head = heads
            .get_or_insert_with(HashMap::new)
            .entry(pos)
            .or_insert(Head { at: centre, time });
        let elapsed = rules::ticks_between(head.time, time).min(20.0);
        head.time = time;
        if working != 0 || has_target == 0 {
            let d = [
                target[0] - head.at[0],
                target[1] - head.at[1],
                target[2] - head.at[2],
            ];
            let distance = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            let pace = (3.0 / ticks.max(1) as f32).max(distance / 40.0);
            head.at = rules::approach(head.at, target, pace * elapsed);
        }
        head.at
    };
    let draw = &mut *event.draw;
    let mut part = |sprite: &str, at: [f32; 3], from: [f32; 3], to: [f32; 3]| {
        let face = Some(CubeFace::new(sprite));
        draw.push();
        draw.translate(at);
        draw.cube(from, to, [face, face, face, face, face, face], 0xFF_FFFF);
        draw.pop();
    };

    // The bridge across the square, at the head's z.
    let mut x = x0;
    while x <= x1 {
        part(FRAME, [x, GANTRY, head[2]], along_x.0, along_x.1);
        x += 1.0;
    }
    // The carriage on the bridge, the shaft below it, the bit at its end.
    part(FRAME, [head[0], GANTRY, head[2]], [3.0; 3], [13.0; 3]);
    let mut y = GANTRY;
    while y - 1.0 >= head[1] {
        y -= 1.0;
        part(DRILL, [head[0], y, head[2]], upright.0, upright.1);
    }
    if y > head[1] {
        let left = (y - head[1]) * 16.0;
        part(
            DRILL,
            [head[0], head[1], head[2]],
            upright.0,
            [10.0, left, 10.0],
        );
    }
    part(
        DRILL,
        [head[0], head[1], head[2]],
        [4.0, 0.0, 4.0],
        [12.0, 6.0, 12.0],
    );
}

// ---- the part ------------------------------------------------------------------

pub(crate) struct Quarry;

#[mod_api::sunrise_part]
impl BlockPart for Quarry {}

#[mod_api::sunrise_part]
impl BlockEntityPart for Quarry {
    fn block_entity_tick(&self, event: &mut BlockEntityTickEvent<'_>) {
        tick(event);
    }

    fn block_entity_update_tag(
        &self,
        event: &mut BlockEntityUpdateTagEvent<'_>,
    ) -> Option<Compound> {
        update_tag(event)
    }

    fn block_entity_render_bounding_box(
        &self,
        _: &dyn ClientHost,
        event: &mut BlockEntityRenderBoundingBoxEvent<'_>,
    ) {
        *event.bounds = [
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
            f32::INFINITY,
            f32::INFINITY,
            f32::INFINITY,
        ];
    }

    fn block_entity_submit(&self, host: &dyn ClientHost, event: &mut BlockEntitySubmitEvent<'_>) {
        submit(host, event);
    }
}
