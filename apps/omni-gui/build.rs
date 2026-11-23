fn main() {
    // Los textos de la interfaz están en español; el inglés va empaquetado en
    // lang/en/LC_MESSAGES/omni-gui.po y se elige en Ajustes.
    println!("cargo:rerun-if-changed=lang");
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .with_bundled_translations("lang")
            .with_default_translation_context(slint_build::DefaultTranslationContext::None),
    )
    .expect("no se pudo compilar la interfaz Slint");

    // Windows: el icono y los datos que se ven en el Explorador y en la
    // barra de tareas van dentro del .exe.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("ui/icon.ico");
        res.set("ProductName", "OmniLoad");
        res.set("FileDescription", "OmniLoad, gestor de descargas");
        res.set("LegalCopyright", "GPL-3.0-or-later");
        res.compile()
            .expect("no se pudo añadir el icono a omniload.exe");
    }
}
