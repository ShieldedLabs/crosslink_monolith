use std::{hash::Hash};
use winit::{event::MouseButton, keyboard::KeyCode};

use super::*;

// make widgets nicer to construct
macro_rules! widget {
    // Base case: only id and flags, no fields
    ($id:expr, $flags:expr) => {{
        Widget {
            id: $id,
            flags: $flags,
            ..Default::default()
        }
    }};

    // Case with fields: id, flags, then key-value pairs
    ($id:expr, $flags:expr, $($field:ident: $value:expr),* $(,)?) => {{
        Widget {
            id: $id,
            flags: $flags,
            $($field: $value,)*
            ..Default::default()
        }
    }};
}

// poor man's bitset
macro_rules! bitset {
    // bitset with no type, defaults to u64
    ($name:ident, $($flag:ident = $value:expr),* $(,)?) => {
        bitset!($name<u64>, $($flag = $value),*);
    };

    // bitset with provided type
    ($name:ident<$type:ty>, $($flag:ident = $value:expr),* $(,)?) => {
        #[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
        pub struct $name($type);

        #[allow(dead_code)]
        impl $name {
            $(pub const $flag: Self = Self($value);)*

            pub const fn has(&self, flag: Self) -> bool {
                self.0 & flag.0 != 0
            }
        }

        impl Into<$type> for $name {
            fn into(self) -> $type {
                self.0
            }
        }

        impl From<$type> for $name {
            fn from(value: $type) -> Self {
                Self(value)
            }
        }

        impl std::ops::BitOr for $name {
            type Output = Self;
            fn bitor(self, rhs: Self) -> Self::Output {
                Self(self.0 | rhs.0)
            }
        }
        impl std::ops::BitOrAssign for $name {
            fn bitor_assign(&mut self, rhs: Self) {
                self.0 |= rhs.0;
            }
        }

        impl std::ops::BitAnd for $name {
            type Output = Self;
            fn bitand(self, rhs: Self) -> Self::Output {
                Self(self.0 & rhs.0)
            }
        }
        impl std::ops::BitAndAssign for $name {
            fn bitand_assign(&mut self, rhs: Self) {
                self.0 &= rhs.0;
            }
        }

        impl std::ops::Not for $name {
            type Output = Self;
            fn not(self) -> Self::Output {
                Self(!self.0)
            }
        }
    };
}

pub fn magic<'a, 'b, T>(mut_ref: &'a mut T) -> &'b mut T {
    let mut_ref = mut_ref as *mut T;
    return unsafe { &mut *mut_ref };
}


#[derive(Debug, Default)]
pub struct SomeDataToKeepAround {
    pub messages:          Vec<String>,
    pub can_send_messages: bool,
}

pub fn demo_of_rendering_stuff_with_context_that_allocates_in_the_background(ui: &mut Context, data: &mut SomeDataToKeepAround) -> bool {
    ui.begin_frame();

    if ui.input().key_pressed(KeyCode::Tab) {
        ui.debug = !ui.debug;
    }
    if ui.input().key_pressed(KeyCode::F5) {
        unsafe {
            if *ui.draw().debug_pixel_inspector == None {
                ui.pixel_inspector_primed = true;
            } else {
                *ui.draw().debug_pixel_inspector = None;
                ui.pixel_inspector_primed = false;
            }
        }
    }
    if ui.pixel_inspector_primed {
        if ui.input().mouse_pressed(MouseButton::Left) {
            unsafe {
                *ui.draw().debug_pixel_inspector = Some((ui.input().mouse_pos().0.clamp(0, ui.draw().window_width) as usize, ui.input().mouse_pos().1.clamp(0, ui.draw().window_height) as usize));
            }
            ui.pixel_inspector_primed = false;
        }
    }

    let mut layout = Rect::new(0f32, 0f32, ui.draw().window_width as f32, ui.draw().window_height as f32);
    let panel_w    = layout.width() * 0.25;
    let inset_amt  = 16.0;

    let left_panel   = ui.container_ex(layout.cut_from_left(panel_w).inset(inset_amt), Flags::DEFAULT_CONTAINER_FLAGS  | Flags::RESIZABLE_X);
    let right_panel  = ui.container_ex(layout.cut_from_right(panel_w).inset(inset_amt), Flags::DEFAULT_CONTAINER_FLAGS | Flags::RESIZABLE_X);

    let center_layout = layout.inset(inset_amt);
    let center_panel  = ui.container_ex(center_layout, Flags::DEFAULT_CONTAINER_FLAGS & !(Flags::DRAW_BACKGROUND | Flags::DRAW_BORDER));
    ui.non_ui_drawable_area = center_layout;

    ui.push_parent(left_panel);
    {
        // ui.layout(&[ Size::PercentOfParent{ amount: 1.0, tolerance: 0.0 } ]);
        if ui.button("Wallet") {
            println!("pressed one!");
        }

        if ui.button("History") {
            println!("pressed two!");
        }

        if ui.button("Finalizers") {
            println!("pressed two!");
        }

        // ui.newline();

        // ui.label("");

        ui.newline_ex(0f32);

        ui.label("0.00000 cTAZ");

        ui.newline_ex(0f32);

        if ui.button("Send") {
        }
        if ui.button_ex("Receive", Flags::DEFAULT_BUTTON_FLAGS | Flags::DISABLED).clicked {
        }
        if ui.button("Stake") {
        }

        let event = ui.textbox("0.00000 cTAZ");

    }
    ui.pop_parent(left_panel);

    ui.push_parent(center_panel);
    {
        // nothing for now
    }
    ui.pop_parent(center_panel);


    ui.push_parent(right_panel);
    {
        let event = ui.textbox_ex("Type here!", Flags::DEFAULT_TEXTBOX_FLAGS | if data.can_send_messages { Flags::KEEP_FOCUS } else { Flags::DISABLED });
        if event.submitted {
            let text = event.text.trim();
            if text.len() > 0 {
                data.messages.push(text.to_string());
            }
        }

        if ui.button("Clear Messages") {
            data.messages.clear();
        }

        if ui.button(if data.can_send_messages { "Disable Textbox" } else { "Enable Textbox" }) {
            data.can_send_messages = !data.can_send_messages;
        }

        if ui.checkbox("I am a checkbox?") {
            println!("box was checked!");
        }

        for (i, message) in data.messages.iter().enumerate() {
            ui.label(&format!("{}##{}", message, i));
            ui.newline_ex(0f32);
        }
    }
    ui.pop_parent(right_panel);

    ui.end_frame();

    if ui.pixel_inspector_primed {
        ui.draw().mono_text_line(0.0, 0.0, 16.0, "Pixel Inspector is Primed! Click to select pixel.", 0xff_00ff00);
    }
    if let Some((x, y)) = unsafe { *ui.draw().debug_pixel_inspector } {
        let x = x as isize; let y = y as isize;
        let mut draw_x = 0;
        let mut draw_y = 0;
        if x < ui.draw().window_width/2 { draw_x = ui.draw().window_width - 256};
        if y < ui.draw().window_height/2 { draw_y = ui.draw().window_height - 256};
        let color = unsafe { *ui.draw().debug_pixel_inspector_last_color };
        ui.draw().rectangle(draw_x as f32, draw_y as f32, draw_x as f32 + 256.0, draw_y as f32 + 256.0, 0xff_000000 | color);
        ui.draw().mono_text_line(draw_x as f32, draw_y as f32, 12.0, &format!("({},{}) = {:X}", x, y, color), 0xff_000000 | (color ^ u32::MAX));
    }

    return false;
}

impl Context {
    pub fn new(style: Style) -> Self {
        Self { default_style: style, ..Default::default() }
    }

    #[track_caller]
    #[inline(always)]
    pub fn container(&mut self, rect: Rect) -> WidgetId {
        return self.container_ex(rect, Flags::DEFAULT_CONTAINER_FLAGS);
    }

    #[track_caller]
    pub fn container_ex(&mut self, rect: Rect, flags: Flags) -> WidgetId {
        let widget = magic(self.make_or_get_widget(flags, None, std::panic::Location::caller()));
        widget.size = (Size::Exact(rect.width()), Size::Exact(rect.height()));
        widget.rel_rect = rect;

        self.update_widget_events(widget);
        return widget.id;
    }

    pub fn push_parent(&mut self, id: WidgetId) {
        self.parents.push(id);
        self.parent_stack.push(id);
    }

    pub fn pop_parent(&mut self, id: WidgetId) {
        let pid = self.parent_stack.pop();
        debug_assert!(id == pid, "parent ids did not match between push/pop");
    }

    #[track_caller]
    #[inline(always)]
    pub fn label(&mut self, label: &str) {
        self.label_ex(label, Flags::DEFAULT_LABEL_FLAGS);
    }

    #[track_caller]
    pub fn label_ex(&mut self, label: &str, flags: Flags) -> WidgetEvents {
        let widget  = magic(self.make_or_get_widget(flags, Some(label), std::panic::Location::caller()));
        let font    = Font(0); // @todo font
        widget.size = (Size::TextContent(font), Size::TextContent(font));
        return self.update_widget_events(widget);
    }

    #[track_caller]
    #[inline(always)]
    pub fn button(&mut self, label: &str) -> bool {
        return self.button_ex(label, Flags::DEFAULT_BUTTON_FLAGS).clicked;
    }

    #[track_caller]
    pub fn button_ex(&mut self, label: &str, flags: Flags) -> WidgetEvents {
        let widget  = magic(self.make_or_get_widget(flags, Some(label), std::panic::Location::caller()));
        let font    = Font(0); // @todo font
        widget.size = (Size::TextContent(font), Size::TextContent(font));
        return self.update_widget_events(widget);
    }

    #[track_caller]
    #[inline(always)]
    pub fn checkbox(&mut self, label: &str) -> bool {
        return self.checkbox_ex(label, Flags::DEFAULT_CHECKBOX_FLAGS).clicked;
    }

    #[track_caller]
    pub fn checkbox_ex(&mut self, label: &str, flags: Flags) -> WidgetEvents {
        let widget  = magic(self.make_or_get_widget(flags, Some(label), std::panic::Location::caller()));
        let font    = Font(0); // @todo font
        widget.size = (Size::PercentOfParent { amount: 1.0, tolerance: 0.5 }, Size::TextContent(font));
        return self.update_widget_events(widget);
    }


    #[track_caller]
    #[inline(always)]
    pub fn textbox(&mut self, label: &str) -> (bool, String) {
        let e = self.textbox_ex(label, Flags::DEFAULT_TEXTBOX_FLAGS);
        return (e.submitted, e.text);
    }

    #[track_caller]
    pub fn textbox_ex(&mut self, label: &str, flags: Flags) -> WidgetEvents {
        let widget  = magic(self.make_or_get_widget(flags, Some(label), std::panic::Location::caller()));
        let font    = Font(0); // @todo font
        widget.size = (Size::PercentOfParent { amount: 1.0, tolerance: 1.0 }, Size::TextContent(font));
        return self.update_widget_events(widget);
    }

    #[track_caller]
    #[inline(always)]
    pub fn newline(&mut self) {
        self.newline_ex(self.get_style().spacing);
    }

    #[track_caller]
    pub fn newline_ex(&mut self, height: f32) { // @todo full rearchitecting because this is not how to do newlines long-term
        let widget  = magic(self.make_or_get_widget(Flags::NONE, None, std::panic::Location::caller()));
        widget.size = (Size::PercentOfParent { amount: 1.0, tolerance: 0.0 }, Size::Exact(height));
        self.update_widget_events(widget);
    }



    pub fn push_style(&mut self, style: Style) {
        self.style_stack.push(style);
    }

    #[track_caller]
    pub fn pop_style(&mut self) {
        self.style_stack.pop();
    }

    pub fn get_style(&self) -> Style {
        if self.style_stack.empty() {
            return self.default_style;
        }
        else {
            return *self.style_stack.peek();
        }
    }


    pub fn begin_frame(&mut self) {
        self.active_widget = WidgetId::INVALID;
        self.hot_widget    = WidgetId::INVALID;

        self.parents.clear();
        self.style_stack.reset();
        self.clip_stack.reset();
        self.parent_stack.reset();

        self.draw_commands.clear();
    }

    pub fn end_frame(&mut self) {
        let total_parents = self.parents.len();

        // @todo speedup!!!!!!!!!

        // first pass, compute the exact size of widgets
        for i in 0..total_parents {
            let widget = magic(self.widgets.get_mut(&self.parents[i]).unwrap());
            self.compute_absolute_widget_rect(widget);
        }
        // second pass, compute widget sizes/positions relative to their parents
        for i in 0..total_parents {
            let widget = magic(self.widgets.get_mut(&self.parents[i]).unwrap());
            self.compute_relative_widget_rect(widget);
        }
        // third pass, we can finally render everything
        for i in 0..total_parents {
            let widget = magic(self.widgets.get_mut(&self.parents[i]).unwrap());
            self.push_widget_draw_commands(widget);
        }

        for cmd in &self.draw_commands {
            match cmd {
                DrawCommand::Rect(rect, Some(radius), color) => {
                    self.draw().rounded_rectangle(rect.x1 as isize, rect.y1 as isize, rect.x2 as isize, rect.y2 as isize, *radius as isize, (*color).into());
                }
                DrawCommand::Rect(rect, None, color) => {
                    self.draw().rectangle(rect.x1, rect.y1, rect.x2, rect.y2, (*color).into());
                }
                DrawCommand::Box(rect, thickness, color) => {
                    let x1 = rect.x1 as isize;
                    let y1 = rect.y1 as isize;
                    let x2 = rect.x2 as isize;
                    let y2 = rect.y2 as isize;
                    let t  = (*thickness / 2.0) as isize;

                    self.draw().rectangle((x1-t) as f32, (y1-t) as f32, (x1+t) as f32, (y2+t) as f32, (*color).into());
                    self.draw().rectangle((x2-t) as f32, (y1-t) as f32, (x2+t) as f32, (y2+t) as f32, (*color).into());
                    self.draw().rectangle((x1-t) as f32, (y1-t) as f32, (x2-t) as f32, (y1+t) as f32, (*color).into());
                    self.draw().rectangle((x1-t) as f32, (y2-t) as f32, (x2-t) as f32, (y2+t) as f32, (*color).into());
                }
                DrawCommand::Circle(x, y, radius, color) => {
                    self.draw().circle(*x as f32, *y as f32, *radius as f32, (*color).into());
                }

                DrawCommand::Text(_, x, y, color, text) => {
                    self.draw().text_line(*x as f32, *y as f32, 24.0, text, (*color).into());
                },

                DrawCommand::Scissor(rect) => {
                    self.draw().set_scissor(rect.x1 as isize, rect.y1 as isize, rect.x2 as isize, rect.y2 as isize);
                }
            }
        }
    }
}

impl Context {
    fn update_widget_events(&mut self, widget: &mut Widget) -> WidgetEvents {
        let mut e = WidgetEvents::default();
        if widget.flags.has(Flags::HIDDEN | Flags::DISABLED) {
            if self.active_widget == widget.id {
                self.active_widget = WidgetId::INVALID;
            }
            if self.hot_widget == widget.id {
                self.hot_widget = WidgetId::INVALID;
            }
            if self.hot_input == widget.id {
                self.hot_input = WidgetId::INVALID;
            }

            return e;
        }

        e.mouse = self.input().mouse_pos();

        if widget.flags.has(Flags::CLICKABLE) {
            if widget.viz_rect.point_within(e.mouse.0 as f32, e.mouse.1 as f32) {
                self.active_widget = widget.id;
            }

            if self.active_widget == widget.id {
                if self.input().mouse_held(MouseButton::Left) {
                    e.pressed = true;
                    self.hot_widget = widget.id;
                }
                e.hovering = true;
            }

            if self.hot_widget == widget.id {
                if self.input().mouse_pressed(MouseButton::Left) {
                    e.clicked = true;
                }
                else if self.input().mouse_pressed(MouseButton::Right) {
                    e.right_clicked = true;
                }

                e.pressed = !(e.clicked || e.right_clicked);
            }
        }

        if widget.flags.has(Flags::CHECKABLE) {
            if e.clicked {
                self.hot_input = widget.id;
                widget.checked = !widget.checked;
            }
        }

        if widget.flags.has(Flags::TYPEABLE) {
            if e.clicked {
                self.hot_input = widget.id;
            }

            if self.hot_input == widget.id {
                if let Some(text) = &self.input().text_input {
                    let before_idx   = &widget.text_buf[0..widget.text_idx];
                    let after_idx    = &widget.text_buf[widget.text_idx..];
                    let mut new_buf = Vec::new();
                    new_buf.extend_from_slice(before_idx);
                    new_buf.extend_from_slice(text);
                    new_buf.extend_from_slice(after_idx);
                    widget.text_buf = new_buf;
                    widget.text_idx += text.len();
                }

                if self.input().key_pressed(KeyCode::ArrowLeft) {
                    if widget.text_idx > 0 {
                        widget.text_idx -= 1;
                    }
                }
                if self.input().key_pressed(KeyCode::ArrowRight) {
                    if widget.text_idx < widget.text_buf.len() {
                        widget.text_idx += 1;
                    }
                }
                if self.input().key_pressed(KeyCode::Backspace) {
                    if widget.text_idx > 0 {
                        widget.text_buf.remove(widget.text_idx-1);
                        widget.text_idx -= 1;
                    }
                }
                if self.input().key_pressed(KeyCode::Delete) {
                    if widget.text_idx < widget.text_buf.len() {
                        widget.text_buf.remove(widget.text_idx);
                    }
                }

                if self.input().key_pressed(KeyCode::Enter) {
                    e.text      = widget.text_buf.iter().fold(String::new(), |s, c| format!("{}{}", s, c)); // Note(Sam): I will pray for forgiveness for this sin.
                    e.submitted = true;

                    if !widget.flags.has(Flags::KEEP_FOCUS) {
                        self.hot_input = WidgetId::INVALID;
                    }

                    widget.text_buf.clear();
                    widget.text_idx = 0;
                }

                if (self.input().key_pressed(KeyCode::Escape))
                || (self.input().mouse_pressed(MouseButton::Left) && self.hot_widget != widget.id)
                {
                    self.hot_input = WidgetId::INVALID;
                }
            }
        }

        if widget.flags.has(Flags::RESIZABLE_X) {
        }

        return e;
    }

    fn compute_absolute_widget_rect(&mut self, widget: &mut Widget) {
        if widget.flags.has(Flags::HIDDEN) {
            return;
        }

        let style = self.get_style();
        let mut computed_width  = 0f32;
        let mut computed_height = 0f32;

        let border_width = if widget.flags.has(Flags::DRAW_BORDER) { style.border_width } else { 0f32 };

        // SumOfChildren is the only thing that requires computing children first
        let must_compute_children_first = matches!(widget.size, (Size::SumOfChildren { .. }, _) | (_, Size::SumOfChildren { .. }));
        if must_compute_children_first {
            for child in &widget.children {
                let child = magic(self.widgets.get_mut(child).unwrap());
                self.compute_absolute_widget_rect(child);
                computed_width  += child.rel_rect.width();
                computed_height += child.rel_rect.height();
            }
        }

        // x-axis size
        match widget.size.0 {
            Size::Exact(width_px) => {
                computed_width = width_px;
            }

            Size::TextContent(_font) => {
                let text_width = self.draw().measure_text_line(24.0 /* @todo font */, &widget.display_text);
                computed_width = text_width as f32 + (style.padding + border_width) * 2f32;
            }

            Size::PercentOfParent { amount: x_pct, tolerance: x_tol } => {
                if let Some(parent_id) = widget.parent {
                    let parent = magic(self.widgets.get_mut(&parent_id).unwrap());
                    computed_width = parent.rel_rect.width() * x_pct - (style.spacing + border_width) * 2f32; // @todo tolerance
                }
            }

            Size::SumOfChildren { tolerance: x_tol } => {
            }

            _ => {}
        }

        // y-axis size
        match widget.size.1 {
            Size::Exact(height_px) => {
                computed_height = height_px;
            }
            Size::TextContent(_font) => {
                computed_height = 24f32 /* @todo font */ + (style.padding + border_width) * 2f32;
            }

            Size::PercentOfParent { amount: y_pct, tolerance: y_tol } => {
                if let Some(parent_id) = widget.parent {
                    let parent = magic(self.widgets.get_mut(&parent_id).unwrap());
                    computed_height = parent.rel_rect.height() * y_pct - (style.spacing + border_width) * 2f32; // @todo tolerance
                }
            }

            Size::SumOfChildren { tolerance: y_tol } => {
            }

            _ => {}
        }

        if !must_compute_children_first {
            for child in &widget.children {
                let child = magic(self.widgets.get_mut(child).unwrap());
                self.compute_absolute_widget_rect(child);
                computed_width  += child.rel_rect.width();
                computed_height += child.rel_rect.height();
            }
        }

        if widget.parent.is_some() {
            widget.rel_rect.x2 = computed_width;
            widget.rel_rect.y2 = computed_height;
        }
    }

    fn compute_relative_widget_rect(&mut self, widget: &mut Widget) {
        let style = self.get_style();

        let border_width = if widget.flags.has(Flags::DRAW_BORDER) { style.border_width } else { 0f32 };

        if let Some(parent_id) = widget.parent {
            let parent = magic(self.widgets.get_mut(&parent_id).unwrap());

            if parent.cursor.0 > parent.interior.x1 &&
               parent.cursor.0 + widget.rel_rect.width() >= parent.interior.x2 {
                parent.cursor.1 += parent.line_height + style.spacing;
                parent.cursor.0 = parent.interior.x1;
                parent.line_height = 0f32;
            }

            widget.interior = widget.rel_rect;
            widget.interior.x1 += parent.cursor.0;
            widget.interior.x2 += parent.cursor.0;
            widget.interior.y1 += parent.cursor.1;
            widget.interior.y2 += parent.cursor.1;

            widget.viz_rect = widget.interior;

            parent.cursor.0 += widget.rel_rect.width() + style.spacing;
            parent.line_height = parent.line_height.max(widget.rel_rect.height());
        }
        else {
            widget.interior = widget.rel_rect;
            widget.viz_rect = widget.rel_rect;
            widget.interior.x1 += style.spacing + border_width;
            widget.interior.x2 -= style.spacing + border_width;
            widget.interior.y1 += style.spacing + border_width;
            widget.interior.y2 -= style.spacing + border_width;
            widget.cursor   = (widget.interior.x1, widget.interior.y1);
        }

        for i in 0..widget.children.len() {
            let child = magic(self.widgets.get_mut(&widget.children[i]).unwrap());
            self.compute_relative_widget_rect(child);
        }
    }

    fn push_widget_draw_commands(&mut self, widget: &mut Widget) {
        if widget.flags.has(Flags::HIDDEN) {
            return;
        }

        let style = self.get_style();

        let border_width = if widget.flags.has(Flags::DRAW_BORDER) { style.border_width } else { 0f32 };

        let mut widget_color = style.background;
        if widget.flags.has(Flags::CLICKABLE | Flags::TYPEABLE) {
            if self.active_widget == widget.id {
                widget_color = widget_color.dim(2.0); // @todo style
            }
            if self.hot_widget == widget.id {
                widget_color = widget_color.dim(0.1); // @todo style
            }
        }

        if widget.flags.has(Flags::DISABLED) {
            widget_color = widget_color.dim(0.5);
        }

        if widget.flags.has(Flags::DRAW_BACKGROUND) {
            let color = if widget.flags.has(Flags::DRAW_BORDER) { style.border } else { widget_color };
            self.draw_commands.push(DrawCommand::Rect(widget.viz_rect, Some(style.rounded_corner_radius as isize), color));

            if widget.flags.has(Flags::DRAW_BORDER) {
                let mut r = widget.viz_rect;
                r.x1 += style.border_width;
                r.x2 -= style.border_width;
                r.y1 += style.border_width;
                r.y2 -= style.border_width;
                self.draw_commands.push(DrawCommand::Rect(r, Some((style.rounded_corner_radius * 7f32 / 8f32) as isize), widget_color));
            }
        }

        if widget.flags.has(Flags::DRAW_PIP) {
            let color = style.foreground;
            self.draw_commands.push(DrawCommand::Circle(widget.viz_rect.x1 - 12.0, widget.viz_rect.y1 + (widget.viz_rect.height() / 2.0) - 1.0, 4.0, color)); // @todo style

            if widget.checked {
                self.draw_commands.push(DrawCommand::Circle(widget.viz_rect.x1 - 12.0, widget.viz_rect.y1 + (widget.viz_rect.height() / 2.0) - 1.0, 2.0, color.dim(0.25))); // @todo style
            }
        }

        if widget.flags.has(Flags::DRAW_MONO_TEXT | Flags::DRAW_SERIF_TEXT) {
            let mut text  = widget.display_text.to_string();
            let mut color = style.foreground;
            if widget.flags.has(Flags::TYPEABLE) {
                if widget.text_buf.len() > 0 {
                    text = widget.text_buf.iter().fold(String::new(), |s, c| format!("{}{}", s, c)); // Note(Sam): I will pray for forgiveness for this sin.
                }
                else {
                    color = style.foreground.dim(0.5); // @todo style
                }
            }

            let color = if widget.flags.has(Flags::DISABLED) { color.dim(0.5) } else { color }; // @todo style
            let font  = Font((widget.flags & Flags::DRAW_SERIF_TEXT).into()); // @todo font
            let (mut x, mut y) = (widget.viz_rect.x1, widget.viz_rect.y1);
            x += border_width + style.padding;
            y += border_width + style.padding;
            self.draw_commands.push(DrawCommand::Text(font, x as isize, y as isize, color, text));
        }

        if widget.flags.has(Flags::TYPEABLE) && self.hot_input == widget.id {
            let text_before_idx = widget.text_buf[0..widget.text_idx].iter().fold(String::new(), |s, c| format!("{}{}", s, c)); // Note(Sam): I will pray for forgiveness for this sin.

            let x1: f32;
            if widget.text_idx == 0 {
                x1 = widget.viz_rect.x1 + 1.0 + border_width + style.padding; // @todo style
            }
            else {
                let width_of_current_text = self.draw().measure_text_line(24.0 /* @todo font */, &text_before_idx) as f32;
                x1 = widget.viz_rect.x1 + border_width + style.padding + width_of_current_text as f32;
            }

            let cursor_rect = Rect::new(x1, widget.viz_rect.y1 + 4.0, x1 + 1.0, widget.viz_rect.y2 - 4.0); // @todo style
            self.draw_commands.push(DrawCommand::Rect(cursor_rect, None, Color::DEBUG_RED)); // @todo style
        }

        if self.debug {
            self.draw_commands.push(DrawCommand::Box(widget.viz_rect, style.border_width, Color::DEBUG_MAGENTA.fade(0.25)));
        }

        for i in 0..widget.children.len() {
            let child = magic(self.widgets.get_mut(&widget.children[i]).unwrap());
            self.push_widget_draw_commands(child);
        }
    }

    #[track_caller]
    fn get_parent_id(&self) -> WidgetId {
        return *self.parent_stack.peek();
    }

    fn get_parent(&mut self, id: WidgetId) -> &mut Widget {
        self.widgets.get_mut(&id).expect("parent widget id did not exist!")
    }

    fn make_or_get_widget(&mut self, flags: Flags, label: Option<&str>, loc: &std::panic::Location) -> &mut Widget {
        let mut h = xxhash3_64::Hasher::new();
        loc.file().hash(&mut h);
        loc.line().hash(&mut h);
        loc.column().hash(&mut h);

        let mut display_text = String::new();
        if let Some(label) = label {
            label.hash(&mut h);

            if let Some(idx) = label.find("##") {
                display_text = label[..idx].to_string();
            }
            else {
                display_text = label.to_string();
            }
        }

        let id = WidgetId(h.finish());
        let parent_id = if !self.parent_stack.empty() {
            let parent_id = self.get_parent_id();
            let parent    = self.get_parent(parent_id);
            parent.children.push(id);
            Some(parent_id)
        } else {
            None
        };

        let widget = self.widgets.entry(id).or_insert_with(|| widget!(id, flags,
            parent:       parent_id,
            display_text: display_text,
        ));

        widget.flags = flags;
        widget.children.clear();

        return widget;
    }

    fn draw(&self)  -> &DrawCtx  { unsafe { &*self.draw  } }
    fn input(&self) -> &InputCtx { unsafe { &*self.input } }
}


#[derive(Debug, Default, Clone)]
pub struct Context {
    pub input: *const InputCtx,
    pub draw:  *const DrawCtx,
    pub debug: bool,
    pub pixel_inspector_primed: bool,
    pub non_ui_drawable_area: Rect,

    pub delta: f64,
    pub default_style: Style,
    pub draw_commands: Vec<DrawCommand>,

    hot_widget:    WidgetId, // (focused, clicked)
    hot_input:     WidgetId, // (focused, clicked)
    active_widget: WidgetId, // (hovered)

    widgets: std::collections::HashMap<WidgetId, Widget>,
    parents: std::vec::Vec<WidgetId>,

    style_stack:  Stack<Style>,
    clip_stack:   Stack<Rect>,
    parent_stack: Stack<WidgetId>,
}

bitset!(Flags<u64>,
    NONE = 0 << 0,

    HIDDEN     = 1 << 1,
    DISABLED   = 1 << 2,

    // HOVERABLE   = 1 <<  9,
    CLICKABLE   = 1 << 10,
    CHECKABLE   = 1 << 11,
    TYPEABLE    = 1 << 12,
    RESIZABLE_X = 1 << 13,
    RESIZABLE_Y = 1 << 14,

    DRAW_MONO_TEXT  = 1 << 17,
    DRAW_SERIF_TEXT = 1 << 18,
    DRAW_BACKGROUND = 1 << 19,
    DRAW_BORDER     = 1 << 20,
    DRAW_PIP        = 1 << 21,

    CLIP_CHILDREN = 1 << 25,
    KEEP_FOCUS    = 1 << 26,


    // Widget Defaults
    //////////////////

    DEFAULT_LABEL_FLAGS = Flags::DRAW_MONO_TEXT.0,

    DEFAULT_BUTTON_FLAGS = Flags::CLICKABLE.0
                         | Flags::DRAW_MONO_TEXT.0
                         | Flags::DRAW_BACKGROUND.0
                         | Flags::DRAW_BORDER.0,

    DEFAULT_CHECKBOX_FLAGS = Flags::CHECKABLE.0
                           | Flags::CLICKABLE.0
                           | Flags::DRAW_MONO_TEXT.0
                           | Flags::DRAW_PIP.0,

    DEFAULT_TEXTBOX_FLAGS = Flags::TYPEABLE.0
                          | Flags::CLICKABLE.0
                          | Flags::DRAW_MONO_TEXT.0
                          | Flags::DRAW_BACKGROUND.0
                          | Flags::DRAW_BORDER.0,

    DEFAULT_CONTAINER_FLAGS = Self::DRAW_MONO_TEXT.0
                            | Self::DRAW_SERIF_TEXT.0
                            | Self::DRAW_BACKGROUND.0
                            | Self::DRAW_BORDER.0,
);

#[derive(Debug, Default, Copy, Clone, Eq, Hash, PartialEq, PartialOrd)]
pub struct WidgetId(u64);

impl WidgetId {
    pub const INVALID: WidgetId = WidgetId(0);
}

#[derive(Debug, Default, Copy, Clone)]
enum Size {
    #[default]
    None,
    Exact(f32),
    TextContent(Font),
    PercentOfParent {
        amount:    f32,
        tolerance: f32,
    },
    SumOfChildren {
        tolerance: f32,
    },
}

#[derive(Debug, Default, Clone)]
struct Widget {
    id:    WidgetId,
    flags: Flags,

    parent:   Option<WidgetId>,
    children: Vec<WidgetId>,
    size:     (Size, Size), // semantic size (x-axis, y-axis)

    // CALCULATED PER FRAME
    rel_rect: Rect,    // absolute rect
    viz_rect: Rect,    // positioned rectangle - used to render
    interior: Rect,    // interior available content rectangle
    cursor:   (f32, f32), // layout cursor
    line_height: f32, // max height of all items on this line

    // CONTROL FIELDS
    display_text: String,

    text_idx: usize,
    text_buf: Vec<char>,

    slider_value:     f32,
    slider_value_min: f32,
    slider_value_max: f32,

    checked: bool,
}

#[derive(Debug, Default, Clone)]
pub struct WidgetEvents {
    mouse:      (isize, isize),
    drag_delta: (isize, isize),

    clicked:        bool,
    right_clicked:  bool,

    pressed:  bool,
    released: bool,
    resizing: bool,
    hovering: bool,

    text:      String,
    submitted: bool,
}

#[derive(Debug, Clone)]
pub enum DrawCommand {
    Scissor(Rect),
    Rect(Rect, Option<isize>, Color),
    Circle(f32, f32, f32, Color),
    Box(Rect, f32, Color),
    Text(Font, isize, isize, Color, String),
}

#[derive(Debug, Default, Copy, Clone)]
pub struct Font(u64);

//////////////////////////////////////

#[derive(Debug, Default, Clone, Copy)]
pub struct Rect {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl Rect {
    pub const ZERO: Self = Self { x1: 0f32, y1: 0f32, x2: 0f32, y2: 0f32 };

    pub fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    pub fn width(&self) -> f32 {
        (self.x2 - self.x1).max(0f32)
    }

    pub fn height(&self) -> f32 {
        (self.y2 - self.y1).max(0f32)
    }

    pub fn point_within(&self, px: f32, py: f32) -> bool {
        return px >= self.x1 && px <= self.x2
            && py >= self.y1 && py <= self.y2;
    }

    pub fn cut_from_left(&mut self, a: f32) -> Self {
        let minx = self.x1;
        self.x1  = self.x2.min(self.x1 + a);
        return Self::new(minx, self.y1, self.x1, self.y2);
    }

    pub fn cut_from_right(&mut self, a: f32) -> Self {
        let maxx = self.x2;
        self.x2  = self.x1.max(self.x2 - a);
        return Self::new(self.x2, self.y1, maxx, self.y2);
    }

    pub fn cut_from_top(&mut self, a: f32) -> Self {
        let miny = self.y1;
        self.y1  = self.y2.min(self.y1 + a);
        return Self::new(self.x1, miny, self.x2, self.y1);
    }

    pub fn cut_from_bottom(&mut self, a: f32) -> Self {
        let maxy = self.y2;
        self.y2  = self.y1.max(self.y2 - a);
        return Self::new(self.x1, self.y2, self.x2, maxy);
    }

    pub fn inset(&self, amount: f32) -> Self {
        Self::new(self.x1 + amount, self.y1 + amount, self.x2 - amount, self.y2 - amount)
    }

    pub fn outset(&self, amount: f32) -> Self {
        Self::new(self.x1 - amount, self.y1 - amount, self.x2 + amount, self.y2 + amount)
    }

    pub fn prepare(&mut self, from: CutFrom) -> Cut {
        return Cut{ rect: self, from }
    }

    pub fn intersect(&self, other: Self) -> Self {
        let x1 = self.x1.max(other.x1);
        let y1 = self.y1.max(other.y1);
        let x2 = self.x2.min(other.x2);
        let y2 = self.y2.min(other.y2);
        Self::new(x1, y1, x2, y2)
    }
}

#[derive(Debug)]
pub struct Cut {
    rect: *mut Rect,
    from: CutFrom,
}

#[derive(Debug, Clone)]
pub enum CutFrom {
    Top,
    Bottom,
    Left,
    Right,
}

impl Cut {
    pub fn make(&self, amount: f32) -> Rect {
        match self.from {
            CutFrom::Top    => unsafe { (*self.rect).cut_from_top(amount)    },
            CutFrom::Bottom => unsafe { (*self.rect).cut_from_bottom(amount) },
            CutFrom::Left   => unsafe { (*self.rect).cut_from_left(amount)   },
            CutFrom::Right  => unsafe { (*self.rect).cut_from_right(amount)  },
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const DEBUG_RED:     Self = Self { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const DEBUG_GREEN:   Self = Self { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
    pub const DEBUG_BLUE:    Self = Self { r: 0.0, g: 0.0, b: 1.0, a: 1.0 };
    pub const DEBUG_YELLOW:  Self = Self { r: 1.0, g: 1.0, b: 0.0, a: 1.0 };
    pub const DEBUG_MAGENTA: Self = Self { r: 1.0, g: 0.0, b: 1.0, a: 1.0 };
    pub const DEBUG_CYAN:    Self = Self { r: 0.0, g: 1.0, b: 1.0, a: 1.0 };

    pub fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    pub fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub fn dim(&self, factor: f32) -> Self {
        Self {
            r: self.r * factor,
            g: self.g * factor,
            b: self.b * factor,
            a: self.a,
        }
    }

    pub fn fade(&self, factor: f32) -> Self {
        Self {
            r: self.r,
            g: self.g,
            b: self.b,
            a: self.a * factor,
        }
    }
}

impl Into<u32> for Color {
    fn into(self) -> u32 {
        ((self.a * 255.0) as u8 as u32) << 24 |
        ((self.r * 255.0) as u8 as u32) << 16 |
        ((self.g * 255.0) as u8 as u32) << 8 |
        ((self.b * 255.0) as u8 as u32)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Style {
    pub background: Color,
    pub foreground: Color,
    pub accent:     Color,
    pub border:     Color,
    pub highlight:  Color,

    pub padding: f32,
    pub spacing: f32,
    pub border_width: f32,
    pub rounded_corner_radius: f32,
}

impl Style {
    pub fn dark() -> Self {
        Self {
            background: Color::rgb(0.1, 0.1, 0.1),
            foreground: Color::rgb(0.9, 0.9, 0.9),
            accent:     Color::rgb(0.5, 0.5, 0.5),
            border:     Color::rgb(0.3, 0.3, 0.3),
            highlight:  Color::rgb(0.2, 0.2, 0.2),

            padding: 4f32,
            spacing: 8f32,
            border_width: 2f32,
            rounded_corner_radius: 8f32,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct Stack<T>(std::vec::Vec<T>);

impl<T> Stack<T> {
    fn new() -> Self {
        Self(std::vec::Vec::new())
    }

    fn push(&mut self, item: T) {
        return self.0.push(item);
    }

    fn empty(&self) -> bool {
        self.0.is_empty()
    }

    #[track_caller]
    fn pop(&mut self) -> T {
        return self.0.pop().expect("stack was empty!");
    }

    #[track_caller]
    fn peek(&self) -> &T {
        return self.0.last().expect("stack was empty!");
    }

    fn peek_mut(&mut self) -> &mut T {
        return self.0.last_mut().expect("stack was empty!");
    }

    fn reset(&mut self) {
        self.0.clear();
    }
}
