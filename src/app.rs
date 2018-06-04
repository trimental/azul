use text_cache::TextId;
use dom::UpdateScreen;
use window::FakeWindow;
use css::{Css, FakeCss};
use resources::AppResources;
use app_state::AppState;
use traits::Layout;
use ui_state::UiState;
use ui_description::UiDescription;

use std::sync::{Arc, Mutex, PoisonError};
use window::{Window, WindowCreateOptions, WindowCreateError, WindowId};
use glium::glutin::Event;
use euclid::TypedScale;
use std::io::Read;
use images::{ImageType};
use image::ImageError;
use font::FontError;
use webrender::api::{RenderApi, HitTestFlags};
use glium::SwapBuffersError;
use std::fmt;

/// Graphical application that maintains some kind of application state
pub struct App<'a, T: Layout> {
    /// The graphical windows, indexed by ID
    windows: Vec<Window<T>>,
    /// The global application state
    pub app_state: AppState<'a, T>,
}

/// Error returned by the `.run()` function
///
/// If the `.run()` function would panic, that would need `T` to
/// implement `Debug`, which is not necessary if we just return an error.
pub enum RuntimeError<T: Layout> {
    // Could not swap the display (drawing error)
    GlSwapError(SwapBuffersError),
    ArcUnlockError,
    MutexPoisonError(PoisonError<T>),
}

impl<T: Layout> From<PoisonError<T>> for RuntimeError<T> {
    fn from(e: PoisonError<T>) -> Self {
        RuntimeError::MutexPoisonError(e)
    }
}

impl<T: Layout> From<SwapBuffersError> for RuntimeError<T> {
    fn from(e: SwapBuffersError) -> Self {
        RuntimeError::GlSwapError(e)
    }
}

impl<T: Layout> fmt::Debug for RuntimeError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

pub(crate) struct FrameEventInfo {
    pub(crate) should_redraw_window: bool,
    pub(crate) should_swap_window: bool,
    pub(crate) should_hittest: bool,
    pub(crate) cur_cursor_pos: (f64, f64),
    pub(crate) new_window_size: Option<(u32, u32)>,
    pub(crate) new_dpi_factor: Option<f32>,
}

impl Default for FrameEventInfo {
    fn default() -> Self {
        Self {
            should_redraw_window: false,
            should_swap_window: false,
            should_hittest: false,
            cur_cursor_pos: (0.0, 0.0),
            new_window_size: None,
            new_dpi_factor: None,
        }
    }
}

impl<'a, T: Layout> App<'a, T> {

    /// Create a new, empty application. This does not open any windows.
    pub fn new(initial_data: T) -> Self {
        Self {
            windows: Vec::new(),
            app_state: AppState::new(initial_data),
        }
    }

    /// Spawn a new window on the screen. If an application has no windows,
    /// the [`run`](#method.run) function will exit immediately.
    pub fn create_window(&mut self, options: WindowCreateOptions, css: Css) -> Result<(), WindowCreateError> {
        let window = Window::new(options, css)?;
        self.app_state.windows.push(FakeWindow {
            state: window.state.clone(),
            css: FakeCss::default(),
            read_only_window: window.display.clone(),
        });
        self.windows.push(window);
        Ok(())
    }

    /// Start the rendering loop for the currently open windows
    /// This is the "main app loop", "main game loop" or whatever you want to call it.
    /// Usually this is the last function you call in your `main()` function, since exiting
    /// it means that the user has closed all windows and wants to close the app.
    ///
    /// When all windows are closed, this function returns the internal data again.
    /// This is useful for ex. CLI application that run procedurally, but then want to
    /// open a window temporarily, to ask for user input in a "nicer" way than a pure
    /// CLI-way.
    ///
    /// This way you can do this:
    ///
    /// ```no_run,ignore
    /// let app = App::new(MyData { username: None, password: None });
    /// app.create_window(WindowCreateOptions::default(), Css::native());
    ///
    /// // pop open a window that asks the user for his username and password...
    /// let MyData { username, password } = app.run();
    ///
    /// // continue the rest of the program here...
    /// println!("username: {:?}, password: {:?}", username, password);
    /// ```
    pub fn run(mut self) -> Result<T, RuntimeError<T>>
    {
        self.run_inner()?;
        let unique_arc = Arc::try_unwrap(self.app_state.data).map_err(|_| RuntimeError::ArcUnlockError)?;
        unique_arc.into_inner().map_err(|e| e.into())
    }

    fn run_inner(&mut self) -> Result<(), RuntimeError<T>> {
        use std::{thread, time::{Duration, Instant}};
        use window::{ReadOnlyWindow, WindowInfo};

        let mut ui_state_cache = Self::initialize_ui_state(&self.windows, &self.app_state);
        let mut ui_description_cache = Self::do_first_redraw(&mut self.windows, &mut self.app_state, &ui_state_cache);

        while !self.windows.is_empty() {

            let time_start = Instant::now();
            let mut closed_windows = Vec::<usize>::new();

            for (idx, ref mut window) in self.windows.iter_mut().enumerate() {

                unsafe {
                    use glium::glutin::GlContext;
                    window.display.gl_window().make_current().unwrap();
                }

                // TODO: move this somewhere else
                let svg_shader = &self.app_state.resources.svg_registry.init_shader(&window.display);

                let window_id = WindowId { id: idx };
                let mut frame_event_info = FrameEventInfo::default();

                window.events_loop.poll_events(|event| {
                    let should_close = process_event(event, &mut frame_event_info);
                    if should_close {
                        closed_windows.push(idx);
                    }
                });

                if frame_event_info.should_swap_window {
                    window.display.swap_buffers()?;
                }

                if frame_event_info.should_hittest {
                    Self::do_hit_test_and_call_callbacks(window, window_id, &mut frame_event_info, &ui_state_cache, &mut self.app_state);
                }

                ui_state_cache[idx] = UiState::from_app_state(&self.app_state, WindowInfo {
                    window_id: WindowId { id: idx },
                    window: ReadOnlyWindow {
                        inner: window.display.clone(),
                    }
                });

                // Update the window state that we got from the frame event (updates window dimensions and DPI)
                window.update_from_external_window_state(&mut frame_event_info);
                // Update the window state every frame that was set by the user
                window.update_from_user_window_state(self.app_state.windows[idx].state.clone());

                if frame_event_info.should_redraw_window {
                    println!("updating window!");
                    ui_description_cache[idx] = UiDescription::from_ui_state(&ui_state_cache[idx], &mut window.css);
                    Self::update_display(&window);
                    render(window, &WindowId { id: idx }, &ui_description_cache[idx], &mut self.app_state.resources, true);
                }
            }

            // Close windows if necessary
            closed_windows.into_iter().for_each(|closed_window_id| {
                ui_state_cache.remove(closed_window_id);
                ui_description_cache.remove(closed_window_id);
                self.windows.remove(closed_window_id);
            });

            // Run deamons and remove them from the even queue if they are finished
            self.app_state.run_all_deamons();

            // Clean up finished tasks, remove them if possible
            self.app_state.clean_up_finished_tasks();

            // Wait until 16ms have passed
            let time_end = Instant::now();
            let diff = time_end - time_start;
            if diff < Duration::from_millis(16) {
                thread::sleep(diff);
            }
        }

        Ok(())
    }

    fn update_display(window: &Window<T>)
    {
        use webrender::api::{Transaction, DeviceUintRect, DeviceUintPoint};

        let mut txn = Transaction::new();
        let bounds = DeviceUintRect::new(DeviceUintPoint::new(0, 0), window.internal.framebuffer_size);

        txn.set_window_parameters(window.internal.framebuffer_size, bounds, window.internal.hidpi_factor);
        window.internal.api.send_transaction(window.internal.document_id, txn);
    }

    fn do_hit_test_and_call_callbacks(
        window: &mut Window<T>,
        window_id: WindowId,
        info: &mut FrameEventInfo,
        ui_state_cache: &[UiState<T>],
        app_state: &mut AppState<T>)
    {
        use dom::UpdateScreen;
        use webrender::api::WorldPoint;

        let cursor_x = info.cur_cursor_pos.0 as f32;
        let cursor_y = info.cur_cursor_pos.1 as f32;
        let point = WorldPoint::new(cursor_x, cursor_y);
        let hit_test_results =  window.internal.api.hit_test(
            window.internal.document_id,
            Some(window.internal.pipeline_id),
            point,
            HitTestFlags::FIND_ALL);

        let mut should_update_screen = UpdateScreen::DontRedraw;

        for item in hit_test_results.items {
            let callback_list_opt = ui_state_cache[window_id.id].node_ids_to_callbacks_list.get(&item.tag.0);
            if let Some(callback_list) = callback_list_opt {
                use window::WindowEvent;
                // TODO: filter by `On` type (On::MouseOver, On::MouseLeave, etc.)
                // Currently, this just invoke all actions
                let window_event = WindowEvent {
                    window: window_id.id,
                    // TODO: currently we don't have information about what DOM node was hit
                    number_of_previous_siblings: None,
                    cursor_relative_to_item: (item.point_in_viewport.x, item.point_in_viewport.y),
                    cursor_in_viewport: (item.point_in_viewport.x, item.point_in_viewport.y),
                };

                for callback_id in callback_list.values() {
                    let update = (ui_state_cache[window_id.id].callback_list[callback_id].0)(app_state, window_event);
                    if update == UpdateScreen::Redraw {
                        should_update_screen = UpdateScreen::Redraw;
                    }
                }
            }
        }

        if should_update_screen == UpdateScreen::Redraw {
            info.should_redraw_window = true;
            // TODO: THIS IS PROBABLY THE WRONG PLACE TO DO THIS!!!
            // Copy the current fake CSS changes to the real CSS, then clear the fake CSS again
            // TODO: .clone() and .clear() can be one operation
            window.css.dynamic_css_overrides = app_state.windows[window_id.id].css.dynamic_css_overrides.clone();
            // clear the dynamic CSS overrides
            app_state.windows[window_id.id].css.clear();
        }
    }

    fn initialize_ui_state(windows: &[Window<T>], app_state: &AppState<'a, T>)
    -> Vec<UiState<T>>
    {
        use window::{ReadOnlyWindow, WindowInfo};

        windows.iter().enumerate().map(|(idx, w)|
            UiState::from_app_state(app_state, WindowInfo {
                window_id: WindowId { id: idx },
                window: ReadOnlyWindow {
                    inner: w.display.clone(),
                }
            })
        ).collect()
    }

    /// First repaint, otherwise the window would be black on startup
    fn do_first_redraw(
        windows: &mut [Window<T>],
        app_state: &mut AppState<'a, T>,
        ui_state_cache: &[UiState<T>])
    -> Vec<UiDescription<T>>
    {
        let mut ui_description_cache = vec![UiDescription::default(); windows.len()];

        for (idx, window) in windows.iter_mut().enumerate() {
            ui_description_cache[idx] = UiDescription::from_ui_state(&ui_state_cache[idx], &mut window.css);
            render(window, &WindowId { id: idx, }, &ui_description_cache[idx], &mut app_state.resources, true);
            window.display.swap_buffers().unwrap();
        }

        ui_description_cache
    }

    /// Add an image to the internal resources
    ///
    /// ## Returns
    ///
    /// - `Ok(Some(()))` if an image with the same ID already exists.
    /// - `Ok(None)` if the image was added, but didn't exist previously.
    /// - `Err(e)` if the image couldn't be decoded
    pub fn add_image<S: Into<String>, R: Read>(&mut self, id: S, data: &mut R, image_type: ImageType)
        -> Result<Option<()>, ImageError>
    {
        self.app_state.add_image(id, data, image_type)
    }

    /// Removes an image from the internal app resources.
    /// Returns `Some` if the image existed and was removed.
    /// If the given ID doesn't exist, this function does nothing and returns `None`.
    pub fn delete_image<S: AsRef<str>>(&mut self, id: S)
        -> Option<()>
    {
        self.app_state.delete_image(id)
    }

    /// Checks if an image is currently registered and ready-to-use
    pub fn has_image<S: AsRef<str>>(&mut self, id: S)
        -> bool
    {
        self.app_state.has_image(id)
    }

    /// Add a font (TTF or OTF) as a resource, identified by ID
    ///
    /// ## Returns
    ///
    /// - `Ok(Some(()))` if an font with the same ID already exists.
    /// - `Ok(None)` if the font was added, but didn't exist previously.
    /// - `Err(e)` if the font couldn't be decoded
    pub fn add_font<S: Into<String>, R: Read>(&mut self, id: S, data: &mut R)
        -> Result<Option<()>, FontError>
    {
        self.app_state.add_font(id, data)
    }

    /// Checks if a font is currently registered and ready-to-use
    pub fn has_font<S: Into<String>>(&mut self, id: S)
        -> bool
    {
        self.app_state.has_font(id)
    }

    /// Deletes a font from the internal app resources.
    ///
    /// ## Arguments
    ///
    /// - `id`: The stringified ID of the font to remove, e.g. `"Helvetica-Bold"`.
    ///
    /// ## Returns
    ///
    /// - `Some(())` if if the image existed and was successfully removed
    /// - `None` if the given ID doesn't exist. In that case, the function does
    ///    nothing.
    ///
    /// Wrapper function for [`AppState::delete_font`]. After this function has been
    /// called, you can be sure that the renderer doesn't know about your font anymore.
    /// This also means that the font needs to be re-parsed if you want to add it again.
    /// Use with care.
    ///
    /// ## Example
    ///
    #[cfg_attr(feature = "no-opengl-tests", doc = " ```no_run")]
    #[cfg_attr(not(feature = "no-opengl-tests"), doc = " ```")]
    /// # use azul::prelude::*;
    /// # const TEST_FONT: &[u8] = include_bytes!("../assets/fonts/weblysleekuil.ttf");
    /// #
    /// # struct MyAppData { }
    /// #
    /// # impl Layout for MyAppData {
    /// #     fn layout(&self, _window_id: WindowId) -> Dom<MyAppData> {
    /// #         Dom::new(NodeType::Div)
    /// #    }
    /// # }
    /// #
    /// # fn main() {
    /// let mut app = App::new(MyAppData { });
    /// app.add_font("Webly Sleeky UI", &mut TEST_FONT).unwrap();
    /// app.delete_font("Webly Sleeky UI");
    /// // NOTE: The font isn't immediately removed, only in the next draw call
    /// app.mock_render_frame();
    /// assert!(!app.has_font("Webly Sleeky UI"));
    /// # }
    /// ```
    ///
    /// [`AppState::delete_font`]: ../app_state/struct.AppState.html#method.delete_font
    pub fn delete_font<S: Into<String>>(&mut self, id: S)
        -> Option<()>
    {
        self.app_state.delete_font(id)
    }

    /// Create a deamon. Does nothing if a deamon with the same ID already exists.
    ///
    /// If the deamon was inserted, returns true, otherwise false
    pub fn add_deamon<S: Into<String>>(&mut self, id: S, deamon: fn(&mut T) -> UpdateScreen)
        -> bool
    {
        self.app_state.add_deamon(id, deamon)
    }

    /// Remove a currently running deamon from running. Does nothing if there is
    /// already a deamon with the same ID
    pub fn delete_deamon<S: AsRef<str>>(&mut self, id: S)
        -> bool
    {
        self.app_state.delete_deamon(id)
    }

    pub fn add_text<S: Into<String>>(&mut self, text: S)
    -> TextId
    {
        self.app_state.add_text(text)
    }

    pub fn delete_text(&mut self, id: TextId) {
        self.app_state.delete_text(id);
    }

    pub fn clear_all_texts(&mut self) {
        self.app_state.clear_all_texts();
    }

    /// Mock rendering function, for creating a hidden window and rendering one frame
    /// Used in unit tests. You **have** to enable software rendering, otherwise,
    /// this function won't work in a headless environment.
    ///
    /// **NOTE**: In a headless environment, such as Travis, you have to use XVFB to
    /// create a fake X11 server. XVFB also has a bug where it loads with the default of
    /// 8-bit greyscale color (see [here]). In order to fix that, you have to run:
    ///
    /// `xvfb-run --server-args "-screen 0 1920x1080x24" cargo test --features "doc-test"`
    ///
    /// [here]: https://unix.stackexchange.com/questions/104914/
    ///
    #[cfg(any(feature = "doc-test"))]
    pub fn mock_render_frame(&mut self) {
        use prelude::*;
        let hidden_create_options = WindowCreateOptions {
            state: WindowState { is_visible: false, .. Default::default() },
            /// force sofware renderer (OSMesa)
            renderer_type: RendererType::Software,
            .. Default::default()
        };
        self.create_window(hidden_create_options, Css::native()).unwrap();
        let mut ui_state_cache = Vec::with_capacity(self.windows.len());
        let mut ui_description_cache = vec![UiDescription::default(); self.windows.len()];

        for (idx, _) in self.windows.iter().enumerate() {
            ui_state_cache.push(UiState::from_app_state(&self.app_state, WindowId { id: idx }));
        }

        for (idx, window) in self.windows.iter_mut().enumerate() {
            ui_description_cache[idx] = UiDescription::from_ui_state(&ui_state_cache[idx], &mut window.css);
            render(window, &WindowId { id: idx, },
                  &ui_description_cache[idx],
                  &mut self.app_state.resources,
                  true);
            window.display.swap_buffers().unwrap();
        }
    }
}

impl<'a, T: Layout + Send + 'static> App<'a, T> {
    /// Tasks, once started, cannot be stopped, which is why there is no `.delete()` function
    pub fn add_task(&mut self, callback: fn(Arc<Mutex<T>>, Arc<()>))
    {
        self.app_state.add_task(callback);
    }
}

fn process_event(event: Event, frame_event_info: &mut FrameEventInfo) -> bool {
    use glium::glutin::WindowEvent;
    match event {
        Event::WindowEvent {
            window_id,
            event
        } => {
            match event {
                WindowEvent::CursorMoved {
                    device_id,
                    position,
                    modifiers,
                } => {
                    frame_event_info.should_hittest = true;
                    frame_event_info.cur_cursor_pos = position;

                    let (_, _, _) = (window_id, device_id, modifiers);
                },
                WindowEvent::Resized(w, h) => {
                    frame_event_info.new_window_size = Some((w, h));
                },
                WindowEvent::Refresh => {
                    frame_event_info.should_redraw_window = true;
                },
                WindowEvent::HiDPIFactorChanged(dpi) => {
                    frame_event_info.new_dpi_factor = Some(dpi);
                },
                WindowEvent::CloseRequested => {
                    return true;
                }
                _ => { },
            }
        },
        Event::Awakened => {
            frame_event_info.should_swap_window = true;
        },
        _ => { },
    }

    false
}

fn render<T: Layout>(
    window: &mut Window<T>,
    _window_id: &WindowId,
    ui_description: &UiDescription<T>,
    app_resources: &mut AppResources<T>,
    has_window_size_changed: bool)
{
    use webrender::api::*;
    use display_list::DisplayList;

    let display_list = DisplayList::new_from_ui_description(ui_description);
    let builder = display_list.into_display_list_builder(
        window.internal.pipeline_id,
        &mut window.solver,
        &mut window.css,
        app_resources,
        &window.internal.api,
        has_window_size_changed);

    if let Some(new_builder) = builder {
        // only finalize the list if we actually need to. Otherwise just redraw the last display list
        window.internal.last_display_list_builder = new_builder.finalize().2;
    }

    let mut txn = Transaction::new();

    txn.set_display_list(
        window.internal.epoch,
        None,
        window.internal.layout_size,
        (window.internal.pipeline_id,
         window.solver.window_dimensions.layout_size,
         window.internal.last_display_list_builder.clone()),
        true,
    );

    txn.set_root_pipeline(window.internal.pipeline_id);
    txn.generate_frame();
    window.internal.api.send_transaction(window.internal.document_id, txn);

    window.renderer.as_mut().unwrap().update();
    window.renderer.as_mut().unwrap().render(window.internal.framebuffer_size).unwrap();
}