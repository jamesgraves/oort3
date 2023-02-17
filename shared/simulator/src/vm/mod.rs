pub mod builtin;
mod limiter;

use crate::debug;
use crate::ship::{ShipClass, ShipHandle};
use crate::simulation::{Code, Simulation};
use nalgebra::point;
use oort_api::{Ability, Class, Line, SystemState, Text};
use serde::{Deserialize, Serialize};
use std::cell::{Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;
use wasmer::{imports, Instance, MemoryView, Module, Store, WasmPtr};

pub type Vec2 = nalgebra::Vector2<f64>;

const GAS_PER_TICK: i32 = 1_000_000;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Error {
    pub msg: String,
}

#[cfg(target_arch = "wasm32")]
impl From<wasm_bindgen::JsValue> for Error {
    fn from(err: wasm_bindgen::JsValue) -> Self {
        Self {
            msg: format!("JS error: {:?}", err),
        }
    }
}

impl From<wasmer::InstantiationError> for Error {
    fn from(err: wasmer::InstantiationError) -> Self {
        Self {
            msg: format!("Wasmer instantiation error: {err:?}"),
        }
    }
}
pub fn new_team_controller(code: &Code) -> Result<Box<TeamController>, Error> {
    match code {
        Code::Wasm(_) => TeamController::create(code),
        #[cfg(feature = "precompile")]
        Code::Precompiled(_) => TeamController::create(code),
        Code::Builtin(name) => match builtin::load_compiled(name) {
            Ok(code) => new_team_controller(&code),
            Err(e) => Err(Error { msg: e }),
        },
        _ => unreachable!(),
    }
}

pub struct TeamController {
    vm: WasmVm,
    ships: HashMap<ShipHandle, ShipController>,
}

pub struct ShipController {
    handle: ShipHandle,
    vm: WasmVm,
    state: LocalSystemState,
}

#[derive(Clone)]
pub struct WasmVm {
    store: Rc<RefCell<wasmer::Store>>,
    memory: wasmer::Memory,
    system_state_ptr: WasmPtr<f64>,
    tick_ship: wasmer::Function,
    delete_ship: wasmer::Function,
    reset_gas: wasmer::Function,
}

#[cfg(feature = "precompile")]
pub fn precompile(code: &[u8]) -> Result<Code, Error> {
    let code = limiter::rewrite(code)?;
    let store = Store::default();
    let module = translate_error(Module::new(&store, code))?;
    Ok(Code::Precompiled(translate_error(module.serialize())?))
}

impl WasmVm {
    fn store(&self) -> Ref<'_, Store> {
        self.store.borrow()
    }

    fn store_mut(&self) -> RefMut<'_, Store> {
        self.store.borrow_mut()
    }

    pub fn memory_view(&self) -> MemoryView {
        self.memory.view(self.store().deref())
    }
}

impl TeamController {
    pub fn create(code: &Code) -> Result<Box<TeamController>, Error> {
        Ok(Box::new(TeamController {
            vm: WasmVm::create(code)?,
            ships: HashMap::new(),
        }))
    }

    pub fn add_ship(&mut self, handle: ShipHandle, sim: &Simulation) -> Result<(), Error> {
        let mut ctrl = ShipController {
            handle,
            vm: self.vm.clone(),
            state: LocalSystemState::new(),
        };

        ctrl.state.set(
            SystemState::Seed,
            (make_seed(sim.seed(), handle) & 0xffffff) as f64,
        );
        if let Some(radar) = sim.ship(handle).data().radar.as_ref() {
            ctrl.state.set(SystemState::RadarHeading, radar.heading);
            ctrl.state.set(SystemState::RadarWidth, radar.width);
            ctrl.state
                .set(SystemState::RadarMinDistance, radar.min_distance);
            ctrl.state
                .set(SystemState::RadarMaxDistance, radar.max_distance);
        }

        self.ships.insert(handle, ctrl);

        Ok(())
    }

    pub fn remove_ship(&mut self, handle: ShipHandle) {
        self.ships.remove(&handle);
    }

    pub fn tick(&mut self, sim: &mut Simulation) {
        let mut handles: Vec<_> = self.ships.keys().cloned().collect();
        handles.sort_by_key(|x| x.0);

        for handle in handles {
            let ctrl = self.ships.get_mut(&handle).unwrap();
            if let Err(e) = ctrl.tick(sim) {
                log::warn!("{}", e.msg);
                sim.ship_mut(handle).explode();
            }
        }
    }
}

impl WasmVm {
    pub fn create(code: &Code) -> Result<WasmVm, Error> {
        let mut store = Store::default();
        let module = match code {
            Code::Wasm(wasm) => {
                let wasm = limiter::rewrite(wasm)?;
                translate_error(Module::new(&store, wasm))?
            }
            #[cfg(feature = "precompile")]
            Code::Precompiled(bytes) => {
                translate_error(unsafe { Module::deserialize(&store, bytes.clone()) })?
            }
            _ => unreachable!(),
        };
        let import_object = imports! {};
        let instance = Instance::new(&mut store, &module, &import_object)?;

        let memory = translate_error(instance.exports.get_memory("memory"))?.clone();
        let system_state_offset: i32 =
            translate_error(instance.exports.get_global("SYSTEM_STATE"))?
                .get(&mut store)
                .i32()
                .unwrap();
        let system_state_ptr: WasmPtr<f64> = WasmPtr::new(system_state_offset as u32);

        let tick_ship = translate_error(instance.exports.get_function("export_tick_ship"))?.clone();
        let delete_ship =
            translate_error(instance.exports.get_function("export_delete_ship"))?.clone();
        let reset_gas = translate_error(instance.exports.get_function("reset_gas"))?.clone();

        Ok(WasmVm {
            store: Rc::new(RefCell::new(store)),
            memory,
            system_state_ptr,
            tick_ship,
            delete_ship,
            reset_gas,
        })
    }
}

impl ShipController {
    pub fn read_system_state(&mut self) {
        let memory_view = self.vm.memory_view();
        let slice = self
            .vm
            .system_state_ptr
            .slice(&memory_view, SystemState::Size as u32)
            .expect("system state read");
        slice
            .read_slice(&mut self.state.state)
            .expect("system state read");
    }

    pub fn write_system_state(&self) {
        let memory_view = self.vm.memory_view();
        let slice = self
            .vm
            .system_state_ptr
            .slice(&memory_view, SystemState::Size as u32)
            .expect("system state write");
        slice
            .write_slice(&self.state.state)
            .expect("system state write");
    }

    pub fn read_string(&self, offset: u32, length: u32) -> Option<String> {
        let memory_view = self.vm.memory_view();
        let ptr: WasmPtr<u8> = WasmPtr::new(offset);
        let mut bytes: Vec<u8> = Vec::new();
        bytes.resize(length as usize, 0);
        let slice = ptr.slice(&memory_view, length).ok()?;
        slice.read_slice(&mut bytes).ok()?;
        String::from_utf8(bytes).ok()
    }

    pub fn read_vec<T: Default + Clone>(&self, offset: u32, length: u32) -> Option<Vec<T>> {
        let memory_view = self.vm.memory_view();
        let ptr: WasmPtr<u8> = WasmPtr::new(offset);
        let byte_length = length.saturating_mul(std::mem::size_of::<T>() as u32);
        let slice = ptr.slice(&memory_view, byte_length).ok()?;
        let byte_vec = slice.read_to_vec().ok()?;
        let src_ptr = unsafe { std::mem::transmute::<*const u8, *const T>(byte_vec.as_ptr()) };
        let src_slice = unsafe { std::slice::from_raw_parts(src_ptr, length as usize) };
        Some(src_slice.to_vec())
    }

    pub fn tick(&mut self, sim: &mut Simulation) -> Result<(), Error> {
        {
            translate_runtime_error(
                self.vm
                    .reset_gas
                    .call(self.vm.store_mut().deref_mut(), &[GAS_PER_TICK.into()]),
            )?;

            generate_system_state(sim, self.handle, &mut self.state);
            self.write_system_state();
        }

        let (index, _) = self.handle.0.into_raw_parts();
        let index = index as i32;
        translate_runtime_error(
            self.vm
                .tick_ship
                .call(self.vm.store_mut().deref_mut(), &[index.into()]),
        )?;

        {
            self.read_system_state();
            apply_system_state(sim, self.handle, &mut self.state);

            if self.state.get(SystemState::DebugTextLength) > 0.0 {
                let offset = self.state.get(SystemState::DebugTextPointer) as u32;
                let length = self.state.get(SystemState::DebugTextLength) as u32;
                if let Some(s) = self.read_string(offset, length) {
                    sim.emit_debug_text(self.handle, s);
                }
            }

            if self.state.get(SystemState::DebugLinesLength) > 0.0 {
                let offset = self.state.get(SystemState::DebugLinesPointer) as u32;
                let length = self.state.get(SystemState::DebugLinesLength) as u32;
                if length <= 128 {
                    if let Some(lines) = self.read_vec::<Line>(offset, length) {
                        if validate_lines(&lines) {
                            sim.emit_debug_lines(
                                self.handle,
                                &lines
                                    .iter()
                                    .map(|v| crate::debug::Line {
                                        a: point![v.x0, v.y0],
                                        b: point![v.x1, v.y1],
                                        color: debug::convert_color(v.color),
                                    })
                                    .collect::<Vec<debug::Line>>(),
                            );
                        }
                    }
                }
            }

            if self.state.get(SystemState::DrawnTextLength) > 0.0 {
                let offset = self.state.get(SystemState::DrawnTextPointer) as u32;
                let length = self.state.get(SystemState::DrawnTextLength) as u32;
                if length <= 128 {
                    if let Some(texts) = self.read_vec::<Text>(offset, length) {
                        if validate_texts(&texts) {
                            sim.emit_drawn_text(self.handle, &texts);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub fn delete(&mut self) {
        let (index, _) = self.handle.0.into_raw_parts();
        let index = index as i32;
        if let Err(e) = translate_runtime_error(
            self.vm
                .delete_ship
                .call(self.vm.store_mut().deref_mut(), &[index.into()]),
        ) {
            log::warn!("Failed to delete ship: {:?}", e);
        }
    }
}

fn make_seed(sim_seed: u32, handle: ShipHandle) -> i64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;
    let mut s = DefaultHasher::new();
    let (i, j) = handle.0.into_raw_parts();
    s.write_u32(sim_seed);
    s.write_u32(i);
    s.write_u32(j);
    s.finish() as i64
}

struct LocalSystemState {
    pub state: [f64; SystemState::Size as usize],
}

impl LocalSystemState {
    pub fn new() -> Self {
        Self {
            state: [0.0; SystemState::Size as usize],
        }
    }

    pub fn get(&self, index: SystemState) -> f64 {
        let v = self.state[index as usize];
        if v.is_nan() || v.is_infinite() {
            0.0
        } else {
            v
        }
    }

    pub fn set(&mut self, index: SystemState, value: f64) {
        self.state[index as usize] = value;
    }
}

fn generate_system_state(sim: &mut Simulation, handle: ShipHandle, state: &mut LocalSystemState) {
    state.set(
        SystemState::Class,
        translate_class(sim.ship(handle).data().class) as u32 as f64,
    );

    let position = sim.ship(handle).position();
    state.set(SystemState::PositionX, position.x);
    state.set(SystemState::PositionY, position.y);

    let velocity = sim.ship(handle).velocity();
    state.set(SystemState::VelocityX, velocity.x);
    state.set(SystemState::VelocityY, velocity.y);

    state.set(SystemState::Heading, sim.ship(handle).heading());
    state.set(
        SystemState::AngularVelocity,
        sim.ship(handle).angular_velocity(),
    );

    if let Some(radar) = sim.ship_mut(handle).data_mut().radar.as_mut() {
        state.set(SystemState::RadarHeading, radar.get_heading());
        state.set(SystemState::RadarWidth, radar.get_width());
        state.set(SystemState::RadarMinDistance, radar.get_min_distance());
        state.set(SystemState::RadarMaxDistance, radar.get_max_distance());

        if let Some(contact) = radar.scan() {
            state.set(SystemState::RadarContactFound, 1.0);
            state.set(SystemState::RadarContactPositionX, contact.position.x);
            state.set(SystemState::RadarContactPositionY, contact.position.y);
            state.set(SystemState::RadarContactVelocityX, contact.velocity.x);
            state.set(SystemState::RadarContactVelocityY, contact.velocity.y);
            state.set(
                SystemState::RadarContactClass,
                translate_class(contact.class) as u32 as f64,
            );
        } else {
            state.set(SystemState::RadarContactFound, 0.0);
        }
    } else if let Some(target) = sim.ship(handle).data().target.as_ref() {
        state.set(SystemState::RadarContactPositionX, target.position.x);
        state.set(SystemState::RadarContactPositionY, target.position.y);
        state.set(SystemState::RadarContactVelocityX, target.velocity.x);
        state.set(SystemState::RadarContactVelocityY, target.velocity.y);
    }

    {
        let ship = sim.ship(handle);
        let data = ship.data();
        state.set(
            SystemState::MaxForwardAcceleration,
            data.max_forward_acceleration,
        );
        state.set(
            SystemState::MaxBackwardAcceleration,
            data.max_backward_acceleration,
        );
        state.set(
            SystemState::MaxLateralAcceleration,
            data.max_lateral_acceleration,
        );
        state.set(
            SystemState::MaxAngularAcceleration,
            data.max_angular_acceleration,
        );
    }

    for (i, radio) in sim.ship(handle).data().radios.iter().enumerate() {
        let idxs = oort_api::prelude::radio_internal::radio_indices(i);
        state.set(idxs.channel, radio.get_channel() as f64);
        if let Some(msg) = radio.get_received() {
            state.set(idxs.receive, 1.0);
            state.set(idxs.data[0], msg[0]);
            state.set(idxs.data[1], msg[1]);
            state.set(idxs.data[2], msg[2]);
            state.set(idxs.data[3], msg[3]);
        } else {
            state.set(idxs.receive, 0.0);
        }
        state.set(idxs.send, 0.0);
    }

    state.set(SystemState::CurrentTick, sim.tick() as f64);
}

fn apply_system_state(sim: &mut Simulation, handle: ShipHandle, state: &mut LocalSystemState) {
    sim.ship_mut(handle).accelerate(Vec2::new(
        state.get(SystemState::AccelerateX),
        state.get(SystemState::AccelerateY),
    ));
    state.set(SystemState::AccelerateX, 0.0);
    state.set(SystemState::AccelerateY, 0.0);

    sim.ship_mut(handle).torque(state.get(SystemState::Torque));
    state.set(SystemState::Torque, 0.0);

    for (i, (aim, fire)) in [
        (SystemState::Aim0, SystemState::Fire0),
        (SystemState::Aim1, SystemState::Fire1),
        (SystemState::Aim2, SystemState::Fire2),
        (SystemState::Aim3, SystemState::Fire3),
    ]
    .iter()
    .enumerate()
    {
        if state.get(*fire) > 0.0 {
            sim.ship_mut(handle).aim(i as i64, state.get(*aim));
            sim.ship_mut(handle).fire(i as i64);
            state.set(*fire, 0.0);
        }
    }

    if let Some(radar) = sim.ship_mut(handle).data_mut().radar.as_mut() {
        radar.set_heading(state.get(SystemState::RadarHeading));
        radar.set_width(state.get(SystemState::RadarWidth));
        radar.set_min_distance(state.get(SystemState::RadarMinDistance));
        radar.set_max_distance(state.get(SystemState::RadarMaxDistance));
    }

    if let Some(ability) = translate_ability(state.get(SystemState::ActivateAbility)) {
        if ability != Ability::None {
            sim.ship_mut(handle).activate_ability(ability);
        }
    }

    if state.get(SystemState::Explode) > 0.0 {
        sim.ship_mut(handle).explode();
        state.set(SystemState::Explode, 0.0);
    }

    for (i, radio) in sim
        .ship_mut(handle)
        .data_mut()
        .radios
        .iter_mut()
        .enumerate()
    {
        let idxs = oort_api::prelude::radio_internal::radio_indices(i);
        radio.set_channel(state.get(idxs.channel) as usize);
        if state.get(idxs.send) != 0.0 {
            let msg = [
                state.get(idxs.data[0]),
                state.get(idxs.data[1]),
                state.get(idxs.data[2]),
                state.get(idxs.data[3]),
            ];
            radio.set_sent(Some(msg));
        }
    }
}

fn translate_class(class: ShipClass) -> Class {
    match class {
        ShipClass::Fighter => Class::Fighter,
        ShipClass::Frigate => Class::Frigate,
        ShipClass::Cruiser => Class::Cruiser,
        ShipClass::Asteroid { .. } => Class::Asteroid,
        ShipClass::Target => Class::Target,
        ShipClass::Missile => Class::Missile,
        ShipClass::Torpedo => Class::Torpedo,
        ShipClass::Planet => Class::Unknown,
    }
}

fn translate_ability(v: f64) -> Option<Ability> {
    let v = v as u32;
    if v == Ability::None as u32 {
        Some(Ability::None)
    } else if v == Ability::Boost as u32 {
        Some(Ability::Boost)
    } else if v == Ability::ShapedCharge as u32 {
        Some(Ability::ShapedCharge)
    } else if v == Ability::Decoy as u32 {
        Some(Ability::Decoy)
    } else if v == Ability::Shield as u32 {
        Some(Ability::Shield)
    } else {
        None
    }
}

fn translate_error<T, U>(err: Result<T, U>) -> Result<T, Error>
where
    U: std::fmt::Debug,
{
    match err {
        Ok(val) => Ok(val),
        Err(err) => Err(Error {
            msg: format!("Wasmer error: {err:?}"),
        }),
    }
}

fn translate_runtime_error<T>(err: Result<T, wasmer::RuntimeError>) -> Result<T, Error> {
    match err {
        Ok(val) => Ok(val),
        Err(err) => Err(Error {
            msg: format!("Ship runtime error: {err}"),
        }),
    }
}

fn validate_floats(vs: &[f64]) -> bool {
    vs.iter().all(|v| v.is_finite())
}

fn validate_lines(lines: &[Line]) -> bool {
    lines
        .iter()
        .all(|l| validate_floats(&[l.x0, l.y0, l.x1, l.y1]))
}

fn validate_texts(texts: &[Text]) -> bool {
    texts
        .iter()
        .all(|t| validate_floats(&[t.x, t.y]) && t.length as usize <= t.text.len())
}
