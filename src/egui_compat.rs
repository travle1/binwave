//! Compatibility helpers for tests that drive egui directly.

#[cfg(test)]
pub trait ContextRun {
    fn run(&self, input: egui::RawInput, f: impl FnMut(&egui::Context, &mut egui::Ui)) -> egui::FullOutput;
}

#[cfg(test)]
impl ContextRun for egui::Context {
    fn run(&self, input: egui::RawInput, mut f: impl FnMut(&egui::Context, &mut egui::Ui)) -> egui::FullOutput {
        let mut output = self.run_ui(input, |ui| f(self, ui));
        // Unit tests inspect shapes directly and do not submit texture updates to a renderer.
        output.textures_delta.clear();
        output
    }
}
