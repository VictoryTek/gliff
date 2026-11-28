# The Windows viewer's UI is egui inside our own winit loop

The Windows viewer needs tabs, a start screen with an address bar, and dialogs, which is too much to hand-draw on a softbuffer window. We draw them with egui, through `egui-winit` and `egui-wgpu` in the event loop we already run, not through `eframe`, because the keyboard grab (`grab.rs`) must see Super and Alt+Tab before anything else does, and `eframe` takes the event loop from us. The video becomes a wgpu texture instead of a softbuffer blit. Window colours follow the system light or dark setting, but the controls are egui's, not native Windows controls.

## Considered Options

- **eframe**: less code, but owning the event loop makes the grab harder.
- **Slint**: brings its own event loop, its own build step and its own licence terms.
- **Native Win32 controls**: they look native, but tabs and dialogs mean a lot of unsafe code.
- **GTK4 on Windows, reusing the Linux viewer**: that viewer is built on VA-API and Vulkan, and shipping GTK on Windows is heavy.
