fn main() {
    slint_build::compile("ui/app.slint").expect("no se pudo compilar la interfaz Slint");

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
