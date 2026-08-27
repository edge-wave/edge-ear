fn main() {
    // Apple linkers refuse the Python symbols an extension module
    // leaves for the interpreter. Emits nothing anywhere else.
    pyo3_build_config::add_extension_module_link_args();
}
