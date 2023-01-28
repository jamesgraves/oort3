use crate::compiler_output_window::CompilerOutputWindow;
use crate::documentation::Documentation;
use crate::editor_window::EditorWindow;
use crate::js;
use crate::leaderboard::Leaderboard;
use crate::leaderboard_window::LeaderboardWindow;
use crate::services;
use crate::simulation_window::SimulationWindow;
use crate::toolbar::Toolbar;
use crate::userid;
use crate::welcome::Welcome;
use gloo_render::{request_animation_frame, AnimationFrame};
use monaco::yew::CodeEditorLink;
use oort_proto::{LeaderboardSubmission, Telemetry};
use oort_simulation_worker::SimAgent;
use oort_simulator::scenario::{self, Status, MAX_TICKS};
use oort_simulator::simulation;
use oort_simulator::simulation::Code;
use oort_simulator::snapshot::Snapshot;
use rand::Rng;
use regex::Regex;
use reqwasm::http::Request;
use serde::Deserialize;
use simulation::PHYSICS_TICK_LENGTH;
use std::collections::HashMap;
use std::rc::Rc;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::{EventTarget, HtmlInputElement};
use yew::events::Event;
use yew::html::Scope;
use yew::prelude::*;
use yew_agent::{Bridge, Bridged};
use yew_router::prelude::*;

const NUM_BACKGROUND_SIMULATIONS: u32 = 10;

fn empty() -> JsValue {
    js_sys::Object::new().into()
}

pub enum Msg {
    Render,
    RegisterSimulationWindowLink(Scope<SimulationWindow>),
    SelectScenario(String),
    SelectScenarioAndStart(String, u32),
    SimulationFinished(Snapshot),
    ReceivedBackgroundSimAgentResponse(oort_simulation_worker::Response, u32),
    EditorAction { team: usize, action: String },
    ShowFeedback,
    DismissOverlay,
    CompileFinished(Vec<Result<Code, String>>),
    CompileSlow,
    SubmitToTournament,
    FormattedCode { team: usize, text: String },
}

enum Overlay {
    #[allow(dead_code)]
    MissionComplete,
    Compiling,
    Feedback,
}

#[derive(Deserialize, Debug, Default)]
struct QueryParams {
    pub seed: Option<u32>,
}

pub struct Game {
    render_handle: Option<AnimationFrame>,
    scenario_name: String,
    background_agents: Vec<Box<dyn Bridge<SimAgent>>>,
    background_snapshots: Vec<(u32, Snapshot)>,
    background_nonce: u32,
    overlay: Option<Overlay>,
    overlay_ref: NodeRef,
    simulation_canvas_ref: NodeRef,
    saw_slow_compile: bool,
    compiler_errors: Option<String>,
    frame: u64,
    last_window_size: (i32, i32),
    last_snapshot: Option<Snapshot>,
    simulation_window_link: Option<Scope<SimulationWindow>>,
    teams: Vec<Team>,
    editor_links: Vec<CodeEditorLink>,
    compilation_cache: HashMap<Code, Code>,
    seed: Option<u32>,
}

pub struct Team {
    editor_link: CodeEditorLink,
    initial_source_code: Code,
    initial_compiled_code: Code,
    running_source_code: Code,
    running_compiled_code: Code,
    current_compiler_decorations: js_sys::Array,
}

#[derive(Properties, PartialEq, Eq)]
pub struct Props {
    pub scenario: String,
    #[prop_or_default]
    pub demo: bool,
    pub version: String,
}

impl Component for Game {
    type Message = Msg;
    type Properties = Props;

    fn create(context: &yew::Context<Self>) -> Self {
        let link2 = context.link().clone();
        let render_handle = Some(request_animation_frame(move |_ts| {
            link2.send_message(Msg::Render)
        }));

        let compilation_cache = HashMap::new();

        let q = parse_query_params(context);

        Self {
            render_handle,
            scenario_name: String::new(),
            background_agents: Vec::new(),
            background_snapshots: Vec::new(),
            background_nonce: 0,
            overlay: None,
            overlay_ref: NodeRef::default(),
            simulation_canvas_ref: NodeRef::default(),
            saw_slow_compile: false,
            compiler_errors: None,
            frame: 0,
            last_window_size: (0, 0),
            last_snapshot: None,
            simulation_window_link: None,
            teams: Vec::new(),
            editor_links: vec![CodeEditorLink::default(), CodeEditorLink::default()],
            compilation_cache,
            seed: q.seed,
        }
    }

    fn update(&mut self, context: &yew::Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::Render => {
                if self.frame % 6 == 0 {
                    // TODO: Use ResizeObserver when stable.
                    let root = gloo_utils::document().document_element().unwrap();
                    let new_size = (root.client_width(), root.client_height());
                    if new_size != self.last_window_size {
                        crate::js::golden_layout::update_size();
                        self.last_window_size = new_size;
                    }
                    for editor_link in &self.editor_links {
                        editor_link.with_editor(|editor| {
                            let ed: &monaco::sys::editor::IStandaloneCodeEditor = editor.as_ref();
                            ed.layout(None);
                        });
                    }
                }
                self.frame += 1;

                if let Some(link) = self.simulation_window_link.as_ref() {
                    link.send_message(crate::simulation_window::Msg::Render);
                }

                let link2 = context.link().clone();
                self.render_handle = Some(request_animation_frame(move |_ts| {
                    link2.send_message(Msg::Render)
                }));

                false
            }
            Msg::RegisterSimulationWindowLink(link) => {
                self.simulation_window_link = Some(link);
                context
                    .link()
                    .send_message(Msg::SelectScenario(context.props().scenario.clone()));
                false
            }
            Msg::SelectScenario(scenario_name) => {
                self.change_scenario(context, &scenario_name, false);
                true
            }
            Msg::SelectScenarioAndStart(scenario_name, seed) => {
                self.seed = Some(seed);
                self.change_scenario(context, &scenario_name, true);
                true
            }
            Msg::SimulationFinished(snapshot) => self.on_simulation_finished(context, snapshot),
            Msg::EditorAction {
                team: _,
                ref action,
            } if action == "oort-execute" => {
                if !is_encrypted(&self.player_team().get_editor_code()) {
                    crate::codestorage::save(
                        &self.scenario_name,
                        &self.player_team().get_editor_code(),
                    );
                }
                for team in self.teams.iter_mut() {
                    team.running_source_code = team.get_editor_code();
                }
                self.start_compile(context);
                true
            }
            Msg::EditorAction { team, ref action } if action == "oort-restore-initial-code" => {
                let mut code = scenario::load(&self.scenario_name)
                    .initial_code()
                    .get(team)
                    .unwrap_or(&Code::None)
                    .clone();
                if let Code::Builtin(name) = code {
                    code = oort_simulator::vm::builtin::load_source(&name).unwrap()
                }
                self.team(team).set_editor_text(&code_to_string(&code));
                false
            }
            Msg::EditorAction { team, ref action } if action == "oort-load-solution" => {
                let mut code = scenario::load(&self.scenario_name).solution();
                if let Code::Builtin(name) = code {
                    code = oort_simulator::vm::builtin::load_source(&name).unwrap()
                }
                self.team(team).set_editor_text(&code_to_string(&code));
                false
            }
            Msg::EditorAction { team, ref action } if action == "oort-format" => {
                let text = self.team(team).get_editor_text();
                let cb = context
                    .link()
                    .callback(move |text| Msg::FormattedCode { team, text });
                services::format(text, cb);
                false
            }
            Msg::EditorAction { team: _, action } => {
                log::info!("Got unexpected editor action {}", action);
                false
            }
            Msg::ReceivedBackgroundSimAgentResponse(
                oort_simulation_worker::Response::Snapshot { snapshot },
                seed,
            ) => {
                if snapshot.nonce == self.background_nonce {
                    if snapshot.status == Status::Running
                        && snapshot.time < (MAX_TICKS as f64 * PHYSICS_TICK_LENGTH)
                    {
                        if !self.background_agents.is_empty() {
                            self.background_agents[seed as usize].send(
                                oort_simulation_worker::Request::Snapshot {
                                    ticks: 100,
                                    nonce: self.background_nonce,
                                },
                            );
                        }
                        false
                    } else {
                        self.background_snapshots.push((seed, snapshot));
                        if let Some(summary) = self.summarize_background_simulations() {
                            let code = self.player_team().running_source_code.clone();
                            services::send_telemetry(Telemetry::FinishScenario {
                                scenario_name: self.scenario_name.clone(),
                                code: code_to_string(&code),
                                ticks: (summary.average_time.unwrap_or(0.0)
                                    / simulation::PHYSICS_TICK_LENGTH)
                                    as u32,
                                code_size: crate::code_size::calculate(&code_to_string(&code)),
                                success: summary.failed_seeds.is_empty(),
                                time: summary.average_time,
                            });
                        }
                        true
                    }
                } else {
                    false
                }
            }
            Msg::ShowFeedback => {
                self.overlay = Some(Overlay::Feedback);
                true
            }
            Msg::DismissOverlay => {
                self.overlay = None;
                self.background_agents.clear();
                self.background_snapshots.clear();
                self.background_nonce = 0;
                self.focus_editor();
                true
            }
            Msg::CompileFinished(results) => {
                if matches!(self.overlay, Some(Overlay::Compiling)) {
                    self.overlay = None;
                }
                if self.compilation_cache.len() > 10 {
                    self.compilation_cache.clear();
                }
                for (team, result) in results.iter().enumerate() {
                    match result {
                        Ok(code) => {
                            self.team_mut(team).display_compiler_errors(&[]);
                            self.team_mut(team).running_compiled_code = code.clone();
                            self.compilation_cache
                                .insert(self.team(team).running_source_code.clone(), code.clone());
                        }
                        Err(error) => {
                            self.team_mut(team)
                                .display_compiler_errors(&make_editor_errors(error));
                            self.team_mut(team).running_compiled_code = Code::None;
                        }
                    }
                }
                let errors: Vec<_> = results
                    .iter()
                    .filter_map(|x| x.as_ref().err())
                    .cloned()
                    .collect();
                if errors.is_empty() {
                    services::send_telemetry(Telemetry::StartScenario {
                        scenario_name: self.scenario_name.clone(),
                        code: code_to_string(&self.player_team().running_source_code),
                    });
                    self.run(context);
                    self.focus_simulation();
                } else {
                    self.compiler_errors = Some(errors.join("\n"));
                    self.focus_editor();
                    js::golden_layout::select_tab("compiler_output");
                }
                true
            }
            Msg::CompileSlow => {
                self.saw_slow_compile = true;
                false
            }
            Msg::FormattedCode { team, text } => {
                self.team(team).set_editor_text_preserving_cursor(&text);
                false
            }
            Msg::SubmitToTournament => {
                services::send_telemetry(Telemetry::SubmitToTournament {
                    scenario_name: self.scenario_name.clone(),
                    code: code_to_string(&self.player_team().running_source_code),
                });
                false
            }
        }
    }

    fn view(&self, context: &yew::Context<Self>) -> Html {
        // For Toolbar
        let navigator = context.link().navigator().unwrap();
        let select_scenario_cb = context.link().callback(move |e: Event| {
            let target: EventTarget = e
                .target()
                .expect("Event should have a target when dispatched");
            let data = target.unchecked_into::<HtmlInputElement>().value();
            navigator.push(&crate::Route::Scenario {
                scenario: data.clone(),
            });
            Msg::SelectScenario(data)
        });
        let show_feedback_cb = context.link().callback(|_| Msg::ShowFeedback);

        // For EditorWindow 0
        let editor_window0_host = gloo_utils::document()
            .get_element_by_id("editor-window-0")
            .expect("a #editor-window element");
        let editor0_link = self.editor_links[0].clone();
        let on_editor0_action = context
            .link()
            .callback(|action| Msg::EditorAction { team: 0, action });

        // For EditorWindow 1
        let editor_window1_host = gloo_utils::document()
            .get_element_by_id("editor-window-1")
            .expect("a #editor-window element");
        let editor1_link = self.editor_links[1].clone();
        let on_editor1_action = context
            .link()
            .callback(|action| Msg::EditorAction { team: 1, action });

        // For SimulationWindow
        let simulation_window_host = gloo_utils::document()
            .get_element_by_id("simulation-window")
            .expect("a #simulation-window element");
        let on_simulation_finished = context.link().callback(Msg::SimulationFinished);
        let register_link = context.link().callback(Msg::RegisterSimulationWindowLink);
        let version = context.props().version.clone();

        // For Welcome
        let welcome_window_host = gloo_utils::document()
            .get_element_by_id("welcome-window")
            .expect("a #welcome-window element");
        let navigator = context.link().navigator().unwrap();
        let select_scenario_cb2 = context.link().batch_callback(move |name: String| {
            navigator.push(&crate::Route::Scenario {
                scenario: name.clone(),
            });
            vec![Msg::SelectScenario(name), Msg::DismissOverlay]
        });

        // For Documentation.
        let documentation_window_host = gloo_utils::document()
            .get_element_by_id("documentation-window")
            .expect("a #documentation-window element");

        // For CompilerOutput.
        let compiler_output_window_host = gloo_utils::document()
            .get_element_by_id("compiler-output-window")
            .expect("a #compiler-output-window element");
        let compiler_errors = self.compiler_errors.clone();

        // For LeaderboardWindow.
        let leaderboard_window_host = gloo_utils::document()
            .get_element_by_id("leaderboard-window")
            .expect("a #leaderboard-window element");

        html! {
        <>
            <Toolbar scenario_name={self.scenario_name.clone()} {select_scenario_cb} show_feedback_cb={show_feedback_cb.clone()} />
            <Welcome host={welcome_window_host} show_feedback_cb={show_feedback_cb.clone()} select_scenario_cb={select_scenario_cb2} />
            <EditorWindow host={editor_window0_host} editor_link={editor0_link} on_editor_action={on_editor0_action} team=0 />
            <EditorWindow host={editor_window1_host} editor_link={editor1_link} on_editor_action={on_editor1_action} team=1 />
            <SimulationWindow host={simulation_window_host} {on_simulation_finished} {register_link} {version} canvas_ref={self.simulation_canvas_ref.clone()} />
            <Documentation host={documentation_window_host} {show_feedback_cb} />
            <CompilerOutputWindow host={compiler_output_window_host} {compiler_errors} />
            <LeaderboardWindow host={leaderboard_window_host} scenario_name={self.scenario_name.clone()} />
            { self.render_overlay(context) }
        </>
        }
    }

    fn rendered(&mut self, _context: &yew::Context<Self>, first_render: bool) {
        if self.overlay.is_some() {
            self.focus_overlay();
        } else if first_render {
            // TODO
            self.focus_editor();
        }
    }
}

struct BackgroundSimSummary {
    count: usize,
    victory_count: usize,
    failed_seeds: Vec<u32>,
    average_time: Option<f64>,
    best_seed: Option<u32>,
    worst_seed: Option<u32>,
    scenario_name: String,
}

impl Game {
    fn on_simulation_finished(&mut self, context: &yew::Context<Self>, snapshot: Snapshot) -> bool {
        let status = snapshot.status;

        if !snapshot.errors.is_empty() {
            self.compiler_errors = Some(format!("Simulation errors: {:?}", snapshot.errors));
            return true;
        }

        if context.props().demo && status != Status::Running {
            context
                .link()
                .send_message(Msg::SelectScenario(context.props().scenario.clone()));
            return false;
        }

        if self.leaderboard_eligible() {
            if let Status::Victory { team: 0 } = status {
                self.background_agents.clear();
                self.background_snapshots.clear();
                self.background_nonce = rand::thread_rng().gen();
                let codes: Vec<_> = self
                    .teams
                    .iter()
                    .map(|x| x.running_compiled_code.clone())
                    .collect();
                for seed in 0..NUM_BACKGROUND_SIMULATIONS {
                    let cb = {
                        let link = context.link().clone();
                        move |e| link.send_message(Msg::ReceivedBackgroundSimAgentResponse(e, seed))
                    };
                    let mut sim_agent = SimAgent::bridge(Rc::new(cb));
                    sim_agent.send(oort_simulation_worker::Request::StartScenario {
                        scenario_name: self.scenario_name.to_owned(),
                        seed,
                        codes: codes.clone(),
                        nonce: self.background_nonce,
                    });
                    self.background_agents.push(sim_agent);
                }

                self.overlay = Some(Overlay::MissionComplete);
            }
        }

        self.last_snapshot = Some(snapshot);
        true
    }

    fn render_overlay(&self, context: &yew::Context<Self>) -> Html {
        let outer_click_cb = context.link().callback(|_| Msg::DismissOverlay);
        let close_overlay_cb = context.link().callback(|_| Msg::DismissOverlay);
        let inner_click_cb = context.link().batch_callback(|e: web_sys::MouseEvent| {
            e.stop_propagation();
            None
        });
        let key_cb = context.link().batch_callback(|e: web_sys::KeyboardEvent| {
            if e.key() == "Escape" {
                Some(Msg::DismissOverlay)
            } else {
                None
            }
        });
        if self.overlay.is_none() {
            return html! {};
        }
        let inner_class = match &self.overlay {
            Some(Overlay::Compiling) => "inner-overlay small-overlay",
            _ => "inner-overlay",
        };

        html! {
            <div ref={self.overlay_ref.clone()} id="overlay"
                onkeydown={key_cb} onclick={outer_click_cb} tabindex="-1">
                <div class={inner_class} onclick={inner_click_cb}>{
                    match &self.overlay {
                        Some(Overlay::MissionComplete) => self.render_mission_complete_overlay(context),
                        Some(Overlay::Compiling) => html! { <h1 class="compiling">{ "Compiling..." }</h1> },
                        Some(Overlay::Feedback) => html! { <crate::feedback::Feedback {close_overlay_cb} /> },
                        None => unreachable!(),
                    }
                }</div>
            </div>
        }
    }

    fn focus_overlay(&self) {
        if let Some(element) = self.overlay_ref.cast::<web_sys::HtmlElement>() {
            element.focus().expect("focusing overlay");
        }
    }

    fn focus_editor(&self) {
        let link = self.editor_links[0].clone();
        let cb = Closure::once_into_js(move || {
            js::golden_layout::select_tab("editor.player");
            link.with_editor(|editor| editor.as_ref().focus());
        });
        gloo_utils::window()
            .set_timeout_with_callback(&cb.into())
            .unwrap();
    }

    fn focus_simulation(&self) {
        let canvas_ref = self.simulation_canvas_ref.clone();
        let cb = Closure::once_into_js(move || {
            js::golden_layout::select_tab("simulation");
            if let Some(element) = canvas_ref.cast::<web_sys::HtmlElement>() {
                element.focus().expect("focusing simulation canvas");
            }
        });
        gloo_utils::window()
            .set_timeout_with_callback(&cb.into())
            .unwrap();
    }

    fn summarize_background_simulations(&self) -> Option<BackgroundSimSummary> {
        if self
            .background_snapshots
            .iter()
            .any(|(_, snapshot)| snapshot.nonce != self.background_nonce)
        {
            log::error!("Found unexpected nonce");
            return None;
        }

        let expected_seeds: Vec<u32> = (0..NUM_BACKGROUND_SIMULATIONS).collect();
        let mut found_seeds: Vec<u32> = self
            .background_snapshots
            .iter()
            .map(|(seed, _)| *seed)
            .collect();
        found_seeds.sort();
        if expected_seeds != found_seeds {
            return None;
        }

        let is_victory = |status: &scenario::Status| matches!(*status, Status::Victory { team: 0 });
        let mut failed_seeds: Vec<u32> = self
            .background_snapshots
            .iter()
            .filter(|(_, snapshot)| !is_victory(&snapshot.status))
            .map(|(seed, _)| *seed)
            .collect();
        failed_seeds.sort();
        let victory_count = self.background_snapshots.len() - failed_seeds.len();
        let average_time: Option<f64> = if victory_count > 0 {
            Some(
                self.background_snapshots
                    .iter()
                    .filter(|(_, snapshot)| is_victory(&snapshot.status))
                    .map(|(_, snapshot)| snapshot.score_time)
                    .sum::<f64>()
                    / victory_count as f64,
            )
        } else {
            None
        };

        let mut victory_seeds_by_time: Vec<_> = self
            .background_snapshots
            .iter()
            .filter(|(_, snapshot)| is_victory(&snapshot.status))
            .map(|(seed, snapshot)| (*seed, snapshot.score_time))
            .collect();
        victory_seeds_by_time.sort_by_key(|(_, time)| (time / PHYSICS_TICK_LENGTH) as i64);
        let best_seed = victory_seeds_by_time.first().map(|(seed, _)| *seed);
        let mut worst_seed = victory_seeds_by_time.last().map(|(seed, _)| *seed);
        if worst_seed == best_seed {
            worst_seed = None;
        }

        Some(BackgroundSimSummary {
            count: found_seeds.len(),
            victory_count,
            failed_seeds,
            average_time,
            best_seed,
            worst_seed,
            scenario_name: self.scenario_name.clone(),
        })
    }

    fn render_mission_complete_overlay(&self, context: &yew::Context<Self>) -> Html {
        let score_time = if let Some(snapshot) = self.last_snapshot.as_ref() {
            snapshot.score_time
        } else {
            0.0
        };
        let source_code = code_to_string(&self.player_team().running_source_code);
        let code_size = crate::code_size::calculate(&source_code);

        let next_scenario = scenario::load(&self.scenario_name).next_scenario();

        let make_seed_link_cb = |seed: u32| {
            let navigator = context.link().navigator().unwrap();
            let scenario_name = self.scenario_name.clone();
            context.link().batch_callback(move |_| {
                let mut query = std::collections::HashMap::<&str, String>::new();
                query.insert("seed", seed.to_string());
                navigator
                    .push_with_query(
                        &crate::Route::Scenario {
                            scenario: scenario_name.clone(),
                        },
                        &query,
                    )
                    .unwrap();
                vec![
                    Msg::DismissOverlay,
                    Msg::SelectScenarioAndStart(scenario_name.clone(), seed),
                ]
            })
        };
        let make_seed_link =
            |seed| html! { <a href="#" onclick={make_seed_link_cb(seed)}>{ seed }</a> };

        let background_status = if let Some(summary) = self.summarize_background_simulations() {
            let next_scenario_link = if summary.failed_seeds.is_empty() {
                match next_scenario {
                    Some(scenario_name) => {
                        let next_scenario_cb = context.link().batch_callback(move |_| {
                            vec![
                                Msg::SelectScenario(scenario_name.clone()),
                                Msg::DismissOverlay,
                            ]
                        });
                        html! { <><br /><a href="#" onclick={next_scenario_cb}>{ "Next mission" }</a></> }
                    }
                    None => {
                        html! {}
                    }
                }
            } else {
                html! {}
            };
            let failures = if summary.failed_seeds.is_empty() {
                html! {}
            } else {
                html! {
                    <>
                    <br />
                    <span>
                        <><b class="error">{ "Your solution did not pass all simulations." }</b><br />{ "Failed seeds: " }</>
                        { summary.failed_seeds.iter().cloned().map(|seed: u32| html! {
                            <>{ make_seed_link(seed) }{ "\u{00a0}" }</>  }).collect::<Html>() }
                    </span>
                    </>
                }
            };

            let best_and_worst_seeds = match (summary.best_seed, summary.worst_seed) {
                (Some(best), Some(worst)) => html! {
                    <><br /><span>{ "Best seed: " }{ make_seed_link(best) }{ " Worst: " }{ make_seed_link(worst) }</span></>
                },
                (Some(best), None) => {
                    html! { <><br /><span>{ "Best seed: " }{ make_seed_link(best) }</span></> }
                }
                _ => html! {},
            };
            let submit_button = if scenario::load(&self.scenario_name).is_tournament()
                && summary.victory_count >= (summary.count as f64 * 0.8) as usize
            {
                let cb = context
                    .link()
                    .batch_callback(move |_| vec![Msg::SubmitToTournament, Msg::DismissOverlay]);
                html! {
                    <>
                        <br /><button onclick={cb}>{ "Submit to tournament" }</button><br/>
                    </>
                }
            } else {
                html! {}
            };
            let leaderboard_submission =
                summary
                    .failed_seeds
                    .is_empty()
                    .then(|| LeaderboardSubmission {
                        userid: userid::get_userid(),
                        username: userid::get_username(),
                        timestamp: chrono::Utc::now(),
                        scenario_name: summary.scenario_name.clone(),
                        code: source_code.clone(),
                        code_size,
                        time: summary.average_time.unwrap(),
                    });
            html! {
                <>
                    <span>{ "Simulations complete: " }{ summary.victory_count }{"/"}{ summary.count }{ " successful" }</span><br />
                    <span>
                        { "Average time: " }
                        {
                            if let Some(average_time) = summary.average_time {
                                format!("{:.2} seconds", average_time)
                            } else {
                                "none".to_string()
                            }
                        }
                    </span>
                    { failures }
                    { best_and_worst_seeds }
                    { submit_button }
                    { next_scenario_link }
                    <br />
                    <Leaderboard scenario_name={ self.scenario_name.clone() }
                        submission={leaderboard_submission} />
                </>
            }
        } else {
            html! { <span>{ "Waiting for simulations (" }{ self.background_snapshots.len() }{ "/" }{ self.background_agents.len() }{ " complete)" }</span> }
        };

        html! {
            <div class="centered">
                <h1>{ "Mission Complete" }</h1>
                { "Time: " }{ format!("{:.2}", score_time) }{ " seconds" }<br/>
                { "Code size: " }{ code_size }{ " bytes" }<br/><br/>
                { background_status }<br/><br/>
                <br/><br/>
            </div>
        }
    }

    pub fn start_compile(&mut self, context: &Context<Self>) {
        self.compiler_errors = None;
        self.overlay = Some(Overlay::Compiling);

        let finished_callback = context.link().callback(Msg::CompileFinished);
        let slow_compile_callback = context.link().callback(|_| Msg::CompileSlow);

        let url = if self.saw_slow_compile {
            services::compiler_url()
        } else {
            services::compiler_vm_url()
        };
        let url = format!("{url}/compile");

        async fn compile(
            url: &str,
            text: String,
            slow_compile_cb: Callback<()>,
        ) -> Result<Code, String> {
            if text.trim().is_empty() {
                return Ok(Code::None);
            }

            let start_time = instant::Instant::now();
            let check_compile_time = || {
                let elapsed = instant::Instant::now() - start_time;
                if elapsed > std::time::Duration::from_millis(3000) {
                    log::info!("Compilation was slow, switching backend to serverless");
                    slow_compile_cb.emit(());
                }
            };

            let result = Request::post(url).body(text).send().await;
            if let Err(e) = result {
                log::error!("Compile error: {}", e);
                check_compile_time();
                return Err(e.to_string());
            }

            let response = result.unwrap();
            if !response.ok() {
                let error = response.text().await.unwrap();
                log::error!("Compile error: {}", error);
                check_compile_time();
                return Err(error);
            }

            let wasm = response.binary().await;
            if let Err(e) = wasm {
                log::error!("Compile error: {}", e);
                check_compile_time();
                return Err(e.to_string());
            }

            let elapsed = instant::Instant::now() - start_time;
            log::info!("Compile succeeded in {:?}", elapsed);
            check_compile_time();
            Ok(Code::Wasm(wasm.unwrap()))
        }

        let source_codes: Vec<_> = self
            .teams
            .iter()
            .map(|team| {
                if team.running_source_code == Code::Rust("".to_string()) {
                    Code::None
                } else if team.running_source_code == team.initial_source_code
                    && team.initial_compiled_code != Code::None
                {
                    team.initial_compiled_code.clone()
                } else if let Some(compiled_code) =
                    self.compilation_cache.get(&team.running_source_code)
                {
                    compiled_code.clone()
                } else {
                    team.running_source_code.clone()
                }
            })
            .collect();

        wasm_bindgen_futures::spawn_local(async move {
            let mut results = vec![];
            for source_code in source_codes {
                let result = match source_code {
                    Code::Rust(text) => compile(&url, text, slow_compile_callback.clone()).await,
                    Code::Builtin(name) => oort_simulator::vm::builtin::load_compiled(&name),
                    other => Ok(other),
                };
                results.push(result);
            }
            finished_callback.emit(results);
        });
    }

    pub fn run(&mut self, _context: &Context<Self>) {
        self.compiler_errors = None;

        let codes: Vec<_> = self
            .teams
            .iter()
            .map(|x| x.running_compiled_code.clone())
            .collect();
        let seed = self.seed.unwrap_or_else(|| rand::thread_rng().gen());

        if let Some(link) = self.simulation_window_link.as_ref() {
            link.send_message(crate::simulation_window::Msg::StartSimulation {
                scenario_name: self.scenario_name.clone(),
                seed,
                codes: codes.to_vec(),
            });
        } else {
            log::error!("Missing SimulationWindow");
        }
        self.background_agents.clear();
        self.background_snapshots.clear();
        self.background_nonce = 0;
    }

    pub fn change_scenario(&mut self, context: &Context<Self>, scenario_name: &str, start: bool) {
        if !self.teams.is_empty() && !is_encrypted(&self.player_team().get_editor_code()) {
            crate::codestorage::save(&self.scenario_name, &self.player_team().get_editor_code());
        }
        self.scenario_name = scenario_name.to_string();
        let codes = crate::codestorage::load(&self.scenario_name);
        let scenario = oort_simulator::scenario::load(&self.scenario_name);

        let to_source_code = |code: &Code| match code {
            Code::Builtin(name) => oort_simulator::vm::builtin::load_source(name).unwrap(),
            _ => code.clone(),
        };

        let mut player_team = Team::new(self.editor_links[0].clone());
        player_team.initial_source_code = to_source_code(&codes[0]);

        if context.props().demo || self.scenario_name == "welcome" {
            let solution = scenario.solution();
            player_team.initial_source_code = to_source_code(&solution);
            player_team.running_source_code = player_team.initial_source_code.clone();
            player_team.running_compiled_code = solution;
        } else if let Some(compiled_code) =
            self.compilation_cache.get(&player_team.initial_source_code)
        {
            if start {
                player_team.running_source_code = player_team.initial_source_code.clone();
                player_team.running_compiled_code = compiled_code.clone();
            }
        }

        if self.scenario_name == "welcome" {
            player_team.initial_source_code = Code::Rust(
                "\
// Welcome to Oort.
// Select a scenario from the list in the top-right of the page.
// If you're new, start with \"tutorial01\"."
                    .to_string(),
            );
        }

        player_team.set_editor_text(&code_to_string(&player_team.initial_source_code));
        self.teams = vec![player_team];

        let enemy_code = if codes.len() > 1 {
            codes[1].clone()
        } else {
            Code::None
        };

        let mut enemy_team = Team::new(self.editor_links[1].clone());
        enemy_team.initial_source_code = to_source_code(&enemy_code);
        enemy_team.running_source_code = to_source_code(&enemy_code);
        enemy_team.initial_compiled_code = enemy_code.clone();
        enemy_team.running_compiled_code = enemy_code;
        enemy_team.set_editor_text(&code_to_string(&enemy_team.initial_source_code));
        self.teams.push(enemy_team);

        crate::js::golden_layout::show_welcome(scenario_name == "welcome");

        self.run(context);
    }

    pub fn team(&self, index: usize) -> &Team {
        self.teams.get(index).expect("Invalid team")
    }

    pub fn team_mut(&mut self, index: usize) -> &mut Team {
        self.teams.get_mut(index).expect("Invalid team")
    }

    pub fn player_team(&self) -> &Team {
        self.team(0)
    }

    pub fn leaderboard_eligible(&self) -> bool {
        for team in &self.teams.as_slice()[1..] {
            if team.running_source_code != team.initial_source_code {
                log::info!("Not eligible for leaderboard due to modified opponent code");
                log::info!("Initial: {:?}", team.initial_source_code);
                log::info!("Running: {:?}", team.running_source_code);
                return false;
            }
        }
        !is_encrypted(&self.player_team().running_source_code)
    }
}

impl Team {
    pub fn new(editor_link: CodeEditorLink) -> Self {
        Self {
            editor_link,
            initial_source_code: Code::None,
            running_source_code: Code::None,
            initial_compiled_code: Code::None,
            running_compiled_code: Code::None,
            current_compiler_decorations: js_sys::Array::new(),
        }
    }

    pub fn get_editor_text(&self) -> String {
        self.editor_link
            .with_editor(|editor| editor.get_model().unwrap().get_value())
            .unwrap()
    }

    pub fn get_editor_code(&self) -> Code {
        str_to_code(&self.get_editor_text())
    }

    pub fn set_editor_text(&self, text: &str) {
        self.editor_link.with_editor(|editor| {
            editor.get_model().unwrap().set_value(text);
        });
        // TODO trigger analyzer run
    }

    pub fn set_editor_text_preserving_cursor(&self, text: &str) {
        self.editor_link.with_editor(|editor| {
            let saved = editor.as_ref().save_view_state();
            editor.get_model().unwrap().set_value(text);
            if let Some(view_state) = saved {
                editor.as_ref().restore_view_state(&view_state);
            }
        });
        // TODO trigger analyzer run
    }

    pub fn display_compiler_errors(&mut self, errors: &[CompilerError]) {
        use monaco::sys::{
            editor::IModelDecorationOptions, editor::IModelDeltaDecoration, IMarkdownString, Range,
        };
        let decorations: Vec<IModelDeltaDecoration> = errors
            .iter()
            .map(|error| {
                let decoration: IModelDeltaDecoration = empty().into();
                decoration.set_range(
                    &Range::new(error.line as f64, 1.0, error.line as f64, 1.0).unchecked_into(),
                );
                let options: IModelDecorationOptions = empty().into();
                options.set_is_whole_line(Some(true));
                options.set_class_name("errorDecoration".into());
                let hover_message: IMarkdownString = empty().into();
                js_sys::Reflect::set(
                    &hover_message,
                    &JsValue::from_str("value"),
                    &JsValue::from_str(&error.msg),
                )
                .unwrap();
                options.set_hover_message(&hover_message);
                decoration.set_options(&options);
                decoration
            })
            .collect();
        let decorations_jsarray = js_sys::Array::new();
        for decoration in decorations {
            decorations_jsarray.push(&decoration);
        }
        self.current_compiler_decorations = self
            .editor_link
            .with_editor(|editor| {
                editor
                    .as_ref()
                    .delta_decorations(&self.current_compiler_decorations, &decorations_jsarray)
            })
            .unwrap();
    }
}

pub fn code_to_string(code: &Code) -> String {
    match code {
        Code::None => "".to_string(),
        Code::Rust(s) => s.clone(),
        Code::Wasm(_) => "// wasm".to_string(),
        Code::Builtin(name) => format!("#builtin:{}", name),
    }
}

pub fn str_to_code(s: &str) -> Code {
    let re = Regex::new(r"#builtin:(.*)").unwrap();
    if let Some(m) = re.captures(s) {
        Code::Builtin(m[1].to_string())
    } else if s.trim().is_empty() {
        Code::None
    } else {
        Code::Rust(s.to_string())
    }
}

fn parse_query_params(context: &Context<Game>) -> QueryParams {
    let location = context.link().location().unwrap();
    match location.query::<QueryParams>() {
        Ok(q) => q,
        Err(e) => {
            log::info!("Failed to parse query params: {:?}", e);
            Default::default()
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompilerError {
    pub line: usize,
    pub msg: String,
}

fn make_editor_errors(error: &str) -> Vec<CompilerError> {
    let re = Regex::new(r"(?m)error.*?: (.*?)$\n.*?ai/src/user.rs:(\d+):").unwrap();
    re.captures_iter(error)
        .map(|m| CompilerError {
            line: m[2].parse().unwrap(),
            msg: m[1].to_string(),
        })
        .collect()
}

pub(crate) fn is_encrypted(code: &Code) -> bool {
    match code {
        Code::Rust(src) => src.starts_with("ENCRYPTED:"),
        _ => false,
    }
}
