pub use wasm_bindgen::JsValue;

extern "js" {
    #[wasm_bindgen(js_namespace = document, js_name = getElementById)]
    pub fn get_element_by_id(id: &str) -> JsValue;

    #[wasm_bindgen(js_namespace = Reflect, js_name = get)]
    pub fn get_property(target: &JsValue, property: &JsValue) -> JsValue;

    #[wasm_bindgen(js_namespace = Reflect, js_name = set)]
    pub fn set_property(target: &JsValue, property: &JsValue, value: &JsValue) -> bool;

    pub fn alert(message: &str);
}
