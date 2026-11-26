use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::Value;

use super::{ContractSource, SourceIndex, index_sources, parse_document};
use crate::error::Result;
use crate::model::ExtractionFailure;

pub(super) struct Documents {
    roots: BTreeMap<String, Value>,
    pub(super) sources: SourceIndex,
}

impl Documents {
    // Carga cada archivo referenciado una vez con el lector de la revisión en curso.
    pub(super) fn load(
        file: &str,
        raw: &str,
        mut read: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self> {
        let mut roots = BTreeMap::new();
        let mut originals = BTreeMap::from([(file.to_owned(), raw.to_owned())]);
        let mut pending = BTreeSet::from([file.to_owned()]);
        let mut visited = pending.clone();
        while let Some(path) = pending.pop_first() {
            let Some(raw) = originals.get(&path) else {
                continue;
            };
            let root = match parse_document(raw) {
                Ok(root) => root,
                Err(err) if path == file => return Err(err),
                Err(_) => continue,
            };
            for child in external_files(&path, &root) {
                if visited.insert(child.clone())
                    && let Some(raw) = read(&child)
                {
                    pending.insert(child.clone());
                    originals.insert(child, raw);
                }
            }
            roots.insert(path, root);
        }

        // Indexa después de insertar las raíces para conservar la identidad de sus nodos.
        let sources = roots
            .iter()
            .flat_map(|(path, root)| index_sources(root, &originals[path], path))
            .collect();
        Ok(Self { roots, sources })
    }

    // Devuelve la raíz del único documento cuya lectura es obligatoria.
    pub(super) fn root(&self, file: &str) -> &Value {
        &self.roots[file]
    }

    // Cuenta las raíces de contrato que se pudieron leer y analizar.
    pub(super) fn file_count(&self) -> usize {
        self.roots.len()
    }

    // Prepara un ámbito de diagnóstico independiente para una operación o ruta.
    pub(super) fn context(&self) -> Resolution<'_> {
        Resolution {
            documents: self,
            diagnostics: Vec::new(),
        }
    }
}

pub(super) type VisitedReferences = HashSet<*const Value>;

pub(super) struct Resolution<'a> {
    documents: &'a Documents,
    pub(super) diagnostics: Vec<(ContractSource, ExtractionFailure)>,
}

impl<'a> Resolution<'a> {
    // Expone el índice de posiciones para los campos y parámetros resueltos.
    pub(super) fn sources(&self) -> &'a SourceIndex {
        &self.documents.sources
    }

    // Busca el archivo y la posición original de un nodo en la revisión.
    pub(super) fn source(&self, node: &Value) -> Option<&'a ContractSource> {
        self.sources().get(&std::ptr::from_ref(node))
    }

    // Sigue punteros usando el archivo del nodo y detecta ciclos incluso entre documentos.
    pub(super) fn follow(
        &mut self,
        mut node: &'a Value,
        visited: &mut VisitedReferences,
    ) -> Option<&'a Value> {
        while let Some(reference) = node.get("$ref") {
            let source = self.documents.sources.get(&std::ptr::from_ref(reference))?;
            let target = reference
                .as_str()
                .and_then(|value| parse_reference(&source.file, value))
                .and_then(|(file, pointer)| self.documents.roots.get(&file)?.pointer(&pointer));
            let Some(target) = target else {
                self.diagnostics.push((
                    source.clone(),
                    ExtractionFailure::OpenApiReferenceUnresolved,
                ));
                return None;
            };
            if !visited.insert(std::ptr::from_ref(target)) {
                self.diagnostics
                    .push((source.clone(), ExtractionFailure::OpenApiReferenceCycle));
                return None;
            }
            // Limita ramas largas para que esquemas recursivos no agoten la pila.
            if visited.len() > 128 {
                self.diagnostics.push((
                    source.clone(),
                    ExtractionFailure::OpenApiReferenceUnresolved,
                ));
                return None;
            }
            node = target;
        }
        Some(node)
    }
}

// Localiza candidatos externos para precargar únicamente rutas confinadas al repositorio.
fn external_files(file: &str, root: &Value) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if let Some(entries) = node.as_object() {
            for (key, value) in entries {
                match key.as_str() {
                    "$ref" => {
                        if let Some((target, _)) = value
                            .as_str()
                            .and_then(|value| parse_reference(file, value))
                            && target != file
                        {
                            files.insert(target);
                        }
                    }
                    _ => pending.push(value),
                }
            }
        } else if let Some(items) = node.as_array() {
            pending.extend(items);
        }
    }
    files
}

// Convierte una referencia de archivo en una ruta Git confinada y un JSON Pointer.
fn parse_reference(file: &str, reference: &str) -> Option<(String, String)> {
    let (path, fragment) = reference.split_once('#').unwrap_or((reference, ""));
    let pointer = decode_reference_component(fragment)?;
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return None;
    }
    if path.is_empty() {
        return Some((file.to_owned(), pointer));
    }
    let path = decode_reference_component(path)?;
    if path.starts_with('/')
        || path.contains(['\\', ':', '?'])
        || path.chars().any(char::is_control)
    {
        return None;
    }
    let mut parts: Vec<&str> = file
        .rsplit_once('/')
        .map_or_else(Vec::new, |(parent, _)| parent.split('/').collect());
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| (parts.join("/"), pointer))
}

// Decodifica porcentajes en URI sin interpretar barras, espacios o signos más de otro modo.
fn decode_reference_component(raw: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            out.push((high * 16 + low) as u8);
        } else {
            out.push(byte);
        }
    }
    String::from_utf8(out).ok()
}
