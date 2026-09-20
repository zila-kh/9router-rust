//! Minimal XML tree, XML Canonicalization, and XMLDSig signature verification
//! used by the native SAML endpoints.
//!
//! This is the Rust equivalent of the `@node-saml/node-saml` 5.1.0 ->
//! `@node-saml/xml-crypto` 6.1.2 stack the pinned upstream delegates to: the
//! element tree keeps literal namespace prefixes (exclusive C14N renders them
//! as written), canonicalization implements the exclusive (20010315) and
//! inclusive C14N algorithms with and without comments, and verification
//! supports the RSA PKCS#1v1.5 signature methods over SHA-1/256/384/512.
//! Every failure is a rejection — a parse or transform mismatch can only fail
//! a response, never accept one.

use base64::{engine::general_purpose::STANDARD, Engine};
use rsa::pkcs1v15::{Signature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::signature::Verifier;
use rsa::RsaPublicKey;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha384, Sha512};

const XMLDSIG_NS: &str = "http://www.w3.org/2000/09/xmldsig#";
const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const EC_INCLUSIVE_NS: &str = "http://www.w3.org/2001/10/xml-exc-c14n#";
const C14N10_NS: &str = "http://www.w3.org/TR/2001/REC-xml-c14n-20010315";
const C14N10_WITH_COMMENTS_NS: &str =
    "http://www.w3.org/TR/2001/REC-xml-c14n-20010315#WithComments";
const C14N11_NS: &str = "http://www.w3.org/2006/12/xml-c14n11";
const C14N11_WITH_COMMENTS_NS: &str = "http://www.w3.org/2006/12/xml-c14n11#WithComments";
const ENVELOPED_NS: &str = "http://www.w3.org/2000/09/xmldsig#enveloped-signature";

const EC_INCLUSIVE_WITH_COMMENTS: &str = "http://www.w3.org/2001/10/xml-exc-c14n#WithComments";

const RSA_SHA1: &str = "http://www.w3.org/2000/09/xmldsig#rsa-sha1";
const RSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
const RSA_SHA384: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha384";
const RSA_SHA512: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha512";

const DIGEST_SHA1: &str = "http://www.w3.org/2000/09/xmldsig#sha1";
const DIGEST_SHA256: &str = "http://www.w3.org/2001/04/xmlenc#sha256";
const DIGEST_SHA384: &str = "http://www.w3.org/2001/04/xmldsig-more#sha384";
const DIGEST_SHA512: &str = "http://www.w3.org/2001/04/xmlenc#sha512";

#[derive(Debug, Clone)]
pub enum XmlNode {
    Element {
        prefix: Option<String>,
        local: String,
        /// Namespace declarations in document order (`xmlns` / `xmlns:*`).
        declared_ns: Vec<(Option<String>, String)>,
        /// Attributes excluding namespace declarations.
        attrs: Vec<XmlAttribute>,
    },
    Text(String),
    Comment(String),
    ProcessingInstruction {
        target: String,
        data: String,
    },
}

#[derive(Debug, Clone)]
pub struct XmlAttribute {
    pub prefix: Option<String>,
    pub local: String,
    pub value: String,
}

/// Arena-backed document tree. Node 0 is the document root.
pub struct XmlDocument {
    pub nodes: Vec<XmlNode>,
    parents: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
}

impl XmlDocument {
    pub fn parse(input: &str) -> Result<Self, String> {
        use xmlparser::{ElementEnd, Token, Tokenizer};

        // XML line-ending normalization happens before tokenizing.
        let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
        let mut doc = XmlDocument {
            nodes: vec![XmlNode::Element {
                prefix: None,
                local: "#document".into(),
                declared_ns: Vec::new(),
                attrs: Vec::new(),
            }],
            parents: vec![None],
            children: vec![Vec::new()],
        };
        let mut stack: Vec<usize> = vec![0];
        // The element opened by ElementStart but not yet confirmed by
        // ElementEnd::Open/Empty; attributes attach to it.
        let mut pending: Option<usize> = None;

        for token in Tokenizer::from(normalized.as_str()) {
            let token = token.map_err(|e| format!("XML parse error: {e}"))?;
            match token {
                Token::Declaration { .. } => {}
                Token::ProcessingInstruction {
                    target, content, ..
                } => {
                    let parent = *stack.last().expect("document root is always present");
                    doc.push(
                        XmlNode::ProcessingInstruction {
                            target: target.as_str().to_string(),
                            data: content.map(|d| d.as_str().to_string()).unwrap_or_default(),
                        },
                        parent,
                    );
                }
                Token::Comment { text, .. } => {
                    let parent = *stack.last().expect("document root is always present");
                    doc.push(XmlNode::Comment(text.as_str().to_string()), parent);
                }
                Token::ElementStart { prefix, local, .. } => {
                    let parent = *stack.last().expect("document root is always present");
                    let element = doc.push(
                        XmlNode::Element {
                            // xmlparser uses an empty span for an absent prefix.
                            prefix: non_empty_prefix(prefix.as_str()),
                            local: local.as_str().to_string(),
                            declared_ns: Vec::new(),
                            attrs: Vec::new(),
                        },
                        parent,
                    );
                    pending = Some(element);
                }
                Token::Attribute {
                    prefix,
                    local,
                    value,
                    ..
                } => {
                    let Some(element) = pending else {
                        return Err("XML attribute outside element".into());
                    };
                    let normalized_value = normalize_attribute_value(value.as_str())
                        .map_err(|e| format!("attribute {local}: {e}"))?;
                    let prefix = non_empty_prefix(prefix.as_str());
                    let XmlNode::Element {
                        declared_ns, attrs, ..
                    } = &mut doc.nodes[element]
                    else {
                        unreachable!("pending node is an element");
                    };
                    match (prefix.as_deref(), local.as_str()) {
                        (None, "xmlns") => declared_ns.push((None, normalized_value)),
                        (Some("xmlns"), local_name) => {
                            declared_ns.push((Some(local_name.to_string()), normalized_value));
                        }
                        _ => attrs.push(XmlAttribute {
                            prefix,
                            local: local.as_str().to_string(),
                            value: normalized_value,
                        }),
                    }
                }
                Token::ElementEnd { end, .. } => match end {
                    ElementEnd::Open => {
                        let element = pending.take().ok_or("XML element end without start")?;
                        stack.push(element);
                    }
                    ElementEnd::Empty => {
                        pending.take().ok_or("XML element end without start")?;
                    }
                    ElementEnd::Close(close_prefix, close_local) => {
                        pending.take();
                        let opened = stack.pop().ok_or("XML close without open")?;
                        let close_prefix = non_empty_prefix(close_prefix.as_str());
                        let XmlNode::Element { prefix, local, .. } = &doc.nodes[opened] else {
                            return Err("XML close on document root".into());
                        };
                        if prefix.as_ref() != close_prefix.as_ref() || local != close_local.as_str()
                        {
                            return Err("mismatched XML closing tag".into());
                        }
                    }
                },
                Token::Text { text } => {
                    let expanded =
                        expand_entities(text.as_str()).map_err(|e| format!("text: {e}"))?;
                    let parent = *stack.last().expect("document root is always present");
                    // Merge adjacent text runs so canonicalization sees one
                    // text node the way a DOM parser would.
                    let merges = match doc.children[parent].last() {
                        Some(&last) => matches!(doc.nodes[last], XmlNode::Text(_)),
                        None => false,
                    };
                    if merges {
                        let last = doc.children[parent].last().copied().unwrap_or(0);
                        if let XmlNode::Text(existing) = &mut doc.nodes[last] {
                            existing.push_str(&expanded);
                        }
                    } else {
                        doc.push(XmlNode::Text(expanded), parent);
                    }
                }
                Token::Cdata { text, .. } => {
                    let parent = *stack.last().expect("document root is always present");
                    doc.push(XmlNode::Text(text.as_str().to_string()), parent);
                }
                Token::DtdStart { .. }
                | Token::EmptyDtd { .. }
                | Token::EntityDeclaration { .. }
                | Token::DtdEnd { .. } => {
                    return Err("DTD/entity declarations are not accepted in SAML responses".into());
                }
            }
        }

        if stack.len() != 1 || pending.is_some() {
            return Err("unbalanced XML document".into());
        }
        Ok(doc)
    }

    fn push(&mut self, node: XmlNode, parent: usize) -> usize {
        let index = self.nodes.len();
        self.nodes.push(node);
        self.parents.push(Some(parent));
        self.children.push(Vec::new());
        self.children[parent].push(index);
        index
    }

    pub fn root_element(&self) -> Option<usize> {
        self.children[0].first().copied()
    }

    pub fn children_of(&self, node: usize) -> &[usize] {
        &self.children[node]
    }

    pub fn parent_of(&self, node: usize) -> Option<usize> {
        self.parents[node]
    }

    pub fn is_element(&self, node: usize) -> bool {
        matches!(self.nodes[node], XmlNode::Element { .. })
    }

    pub fn element_prefix(&self, node: usize) -> Option<&str> {
        match &self.nodes[node] {
            XmlNode::Element { prefix, .. } => prefix.as_deref(),
            _ => None,
        }
    }

    pub fn element_local(&self, node: usize) -> &str {
        match &self.nodes[node] {
            XmlNode::Element { local, .. } => local,
            _ => "",
        }
    }

    pub fn attrs_of(&self, node: usize) -> &[XmlAttribute] {
        match &self.nodes[node] {
            XmlNode::Element { attrs, .. } => attrs,
            _ => &[],
        }
    }

    pub fn text_content(&self, node: usize) -> String {
        let mut out = String::new();
        for &child in &self.children[node] {
            match &self.nodes[child] {
                XmlNode::Text(text) => out.push_str(text),
                XmlNode::Element { .. } => out.push_str(&self.text_content(child)),
                _ => {}
            }
        }
        out
    }

    /// All namespace declarations in scope for `node`, nearest declaration
    /// winning. The implicit `xml` binding is included.
    pub fn in_scope_namespaces(&self, node: usize) -> Vec<(Option<String>, String)> {
        let mut out: Vec<(Option<String>, String)> =
            vec![(Some("xml".to_string()), XML_NS.to_string())];
        let mut current = Some(node);
        while let Some(index) = current {
            if let XmlNode::Element { declared_ns, .. } = &self.nodes[index] {
                for (prefix, uri) in declared_ns {
                    if !out.iter().any(|(seen, _)| seen == prefix) {
                        out.push((prefix.clone(), uri.clone()));
                    }
                }
            }
            current = self.parents[index];
        }
        out
    }

    pub fn resolve_prefix(&self, node: usize, prefix: Option<&str>) -> Option<String> {
        self.in_scope_namespaces(node)
            .into_iter()
            .find(|(p, _)| p.as_deref() == prefix)
            .map(|(_, uri)| uri)
    }

    pub fn element_namespace(&self, node: usize) -> Option<String> {
        self.resolve_prefix(node, self.element_prefix(node))
    }

    /// Namespace URI of an attribute. Unprefixed attributes never inherit the
    /// default namespace.
    pub fn attribute_namespace(&self, node: usize, attr: &XmlAttribute) -> Option<String> {
        match &attr.prefix {
            Some(prefix) => self.resolve_prefix(node, Some(prefix)),
            None => None,
        }
    }

    /// Elements in document order whose `ID` attribute equals `id`.
    pub fn elements_with_id(&self, id: &str) -> Vec<usize> {
        let mut out = Vec::new();
        for index in 1..self.nodes.len() {
            if !self.is_element(index) {
                continue;
            }
            if self
                .attrs_of(index)
                .iter()
                .any(|attr| attr.prefix.is_none() && attr.local == "ID" && attr.value == id)
            {
                out.push(index);
            }
        }
        out
    }

    pub fn child_elements(&self, node: usize) -> Vec<usize> {
        self.children[node]
            .iter()
            .copied()
            .filter(|&child| self.is_element(child))
            .collect()
    }

    pub fn first_child_element_named(
        &self,
        node: usize,
        namespace: Option<&str>,
        local: &str,
    ) -> Option<usize> {
        self.child_elements(node).into_iter().find(|&child| {
            self.element_local(child) == local
                && self.element_namespace(child).as_deref() == namespace
        })
    }

    /// All descendant elements in breadth-first (approximate document) order.
    pub fn descendants(&self, node: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut queue = vec![node];
        while let Some(index) = queue.pop() {
            for &child in &self.children[index] {
                if self.is_element(child) {
                    out.push(child);
                    queue.push(child);
                }
            }
        }
        out
    }
}

fn non_empty_prefix(prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        None
    } else {
        Some(prefix.to_string())
    }
}

fn expand_entities(input: &str) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let rest_after = &rest[amp + 1..];
        let Some(semi) = rest_after.find(';') else {
            return Err("unterminated entity reference".into());
        };
        let entity = &rest_after[..semi];
        let replacement = match entity {
            "amp" => "&".to_string(),
            "lt" => "<".to_string(),
            "gt" => ">".to_string(),
            "quot" => "\"".to_string(),
            "apos" => "'".to_string(),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or("invalid character reference")?
                    .to_string()
            }
            _ if entity.starts_with('#') => entity[1..]
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .ok_or("invalid character reference")?
                .to_string(),
            _ => return Err(format!("undefined entity reference &{entity};")),
        };
        out.push_str(&replacement);
        rest = &rest_after[semi + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// XML 1.0 attribute-value normalization: whitespace characters become spaces,
/// character references survive as their character.
fn normalize_attribute_value(input: &str) -> Result<String, String> {
    let expanded = expand_entities(input)?;
    Ok(expanded
        .chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .collect())
}

fn escape_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#xD;"),
            other => out.push(other),
        }
    }
    out
}

fn escape_attribute(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            other => out.push(other),
        }
    }
    out
}

pub struct C14nConfig {
    pub exclusive: bool,
    pub with_comments: bool,
    /// InclusiveNamespaces PrefixList entries (`#default` normalized to "").
    pub prefix_list: Vec<String>,
}

/// Namespaces already rendered by output ancestors. Entries are truncated on
/// unwind, so each element's additions disappear with it.
struct RenderedNamespaces(Vec<(Option<String>, String)>);

impl RenderedNamespaces {
    fn has(&self, prefix: &Option<String>, uri: &str) -> bool {
        self.0.iter().any(|(p, u)| p == prefix && u == uri)
    }
    fn has_default(&self) -> bool {
        self.0.iter().any(|(p, _)| p.is_none())
    }
}

/// Serialize the subtree at `element` in canonical form. The node `skip`
/// (the enveloped signature) and its subtree are omitted.
pub fn canonicalize(
    doc: &XmlDocument,
    element: usize,
    skip: Option<usize>,
    config: &C14nConfig,
) -> String {
    let mut out = String::new();
    let mut rendered = RenderedNamespaces(Vec::new());
    serialize_element(doc, element, skip, config, &mut rendered, &mut out);
    out
}

fn serialize_element(
    doc: &XmlDocument,
    element: usize,
    skip: Option<usize>,
    config: &C14nConfig,
    rendered: &mut RenderedNamespaces,
    out: &mut String,
) {
    let prefix = doc.element_prefix(element).map(str::to_string);
    let local = doc.element_local(element).to_string();
    let attrs: Vec<XmlAttribute> = doc.attrs_of(element).to_vec();
    let in_scope = doc.in_scope_namespaces(element);

    let mut to_render: Vec<(Option<String>, String)> = Vec::new();
    let render = |to_render: &mut Vec<(Option<String>, String)>, used: Option<String>| {
        if let Some((_, uri)) = in_scope.iter().find(|(p, _)| p == &used) {
            if !rendered.has(&used, uri) && !to_render.iter().any(|(p, _)| p == &used) {
                to_render.push((used, uri.clone()));
            }
        }
    };
    if config.exclusive {
        // Exclusive C14N renders only visibly-utilized prefixes: the element's
        // own prefix, attribute prefixes, and the PrefixList entries.
        render(&mut to_render, prefix.clone());
        for attr in &attrs {
            if let Some(attr_prefix) = &attr.prefix {
                render(&mut to_render, Some(attr_prefix.clone()));
            }
        }
        for entry in &config.prefix_list {
            let used = if entry == "#default" {
                None
            } else {
                Some(entry.clone())
            };
            render(&mut to_render, used);
        }
    } else {
        for (scope_prefix, _) in in_scope.clone() {
            if scope_prefix.as_deref() == Some("xml") {
                continue;
            }
            render(&mut to_render, scope_prefix);
        }
    }
    // Fixup: a rendered default namespace must be explicitly cleared
    // (xmlns="") when this element has none in scope.
    if rendered.has_default() && !in_scope.iter().any(|(p, _)| p.is_none()) {
        to_render.push((None, String::new()));
    }

    // C14N namespace ordering: default namespace first, then by prefix.
    to_render.sort_by(|a, b| match (&a.0, &b.0) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(y),
    });

    // Attribute ordering: by (namespace URI, local name, prefix).
    let mut sorted_attrs: Vec<(&XmlAttribute, String)> = attrs
        .iter()
        .map(|attr| {
            let uri = doc.attribute_namespace(element, attr).unwrap_or_default();
            (attr, uri)
        })
        .collect();
    sorted_attrs.sort_by(|a, b| {
        a.1.cmp(&b.1)
            .then_with(|| a.0.local.cmp(&b.0.local))
            .then_with(|| a.0.prefix.cmp(&b.0.prefix))
    });

    out.push('<');
    if let Some(prefix) = &prefix {
        out.push_str(prefix);
        out.push(':');
    }
    out.push_str(&local);
    for (ns_prefix, uri) in &to_render {
        out.push_str(" xmlns");
        if let Some(prefix) = ns_prefix {
            out.push(':');
            out.push_str(prefix);
        }
        out.push_str("=\"");
        out.push_str(&escape_attribute(uri));
        out.push('"');
    }
    for (attr, _) in &sorted_attrs {
        out.push(' ');
        if let Some(attr_prefix) = &attr.prefix {
            out.push_str(attr_prefix);
            out.push(':');
        }
        out.push_str(&attr.local);
        out.push_str("=\"");
        out.push_str(&escape_attribute(&attr.value));
        out.push('"');
    }
    out.push('>');

    let rendered_mark = rendered.0.len();
    for (ns_prefix, uri) in &to_render {
        rendered.0.push((ns_prefix.clone(), uri.clone()));
    }

    for &child in doc.children_of(element) {
        if child == skip.unwrap_or(usize::MAX) {
            continue;
        }
        match &doc.nodes[child] {
            XmlNode::Element { .. } => serialize_element(doc, child, skip, config, rendered, out),
            XmlNode::Text(text) => out.push_str(&escape_text(text)),
            XmlNode::Comment(comment) => {
                if config.with_comments {
                    out.push_str("<!--");
                    out.push_str(comment);
                    out.push_str("-->");
                }
            }
            XmlNode::ProcessingInstruction { target, data } => {
                out.push_str("<?");
                out.push_str(target);
                if !data.is_empty() {
                    out.push(' ');
                    out.push_str(data);
                }
                out.push_str("?>");
            }
        }
    }

    out.push_str("</");
    if let Some(prefix) = &prefix {
        out.push_str(prefix);
        out.push(':');
    }
    out.push_str(&local);
    out.push('>');

    rendered.0.truncate(rendered_mark);
}

// ---------------------------------------------------------------------------
// XMLDSig signature verification
// ---------------------------------------------------------------------------

/// Parsed `ds:Signature` relevant to its single Reference.
struct ParsedSignature {
    signature_node: usize,
    signed_info: usize,
    canonicalization: C14nConfig,
    signature_method: String,
    signature_value: Vec<u8>,
    reference_uri: String,
    transforms: Vec<Transform>,
    digest_method: String,
    digest_value: Vec<u8>,
}

struct Transform {
    algorithm: String,
    prefix_list: Vec<String>,
}

fn parse_prefix_list(doc: &XmlDocument, node: usize) -> Vec<String> {
    doc.descendants(node)
        .into_iter()
        .find(|&n| doc.element_local(n) == "InclusiveNamespaces")
        .map(|n| {
            doc.attrs_of(n)
                .iter()
                .find(|a| a.local == "PrefixList")
                .map(|a| {
                    a.value
                        .split_whitespace()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

fn c14n_config(doc: &XmlDocument, method_node: usize) -> Option<C14nConfig> {
    let algorithm = doc
        .attrs_of(method_node)
        .iter()
        .find(|a| a.local == "Algorithm")
        .map(|a| a.value.clone())?;
    let (exclusive, with_comments) = match algorithm.as_str() {
        EC_INCLUSIVE_NS => (true, false),
        EC_INCLUSIVE_WITH_COMMENTS => (true, true),
        C14N10_NS => (false, false),
        C14N10_WITH_COMMENTS_NS => (false, true),
        C14N11_NS => (false, false),
        C14N11_WITH_COMMENTS_NS => (false, true),
        _ => return None,
    };
    Some(C14nConfig {
        exclusive,
        with_comments,
        prefix_list: parse_prefix_list(doc, method_node),
    })
}

fn parse_signature(doc: &XmlDocument, signature_node: usize) -> Result<ParsedSignature, String> {
    let signed_info = doc
        .first_child_element_named(signature_node, Some(XMLDSIG_NS), "SignedInfo")
        .ok_or("signature has no SignedInfo")?;
    let signed_value_node = doc
        .first_child_element_named(signature_node, Some(XMLDSIG_NS), "SignatureValue")
        .ok_or("signature has no SignatureValue")?;

    let canon_node = doc
        .first_child_element_named(signed_info, Some(XMLDSIG_NS), "CanonicalizationMethod")
        .ok_or("SignedInfo has no CanonicalizationMethod")?;
    let canonicalization =
        c14n_config(doc, canon_node).ok_or("unsupported canonicalization method")?;

    let method_node = doc
        .first_child_element_named(signed_info, Some(XMLDSIG_NS), "SignatureMethod")
        .ok_or("SignedInfo has no SignatureMethod")?;
    let signature_method = doc
        .attrs_of(method_node)
        .iter()
        .find(|a| a.local == "Algorithm")
        .map(|a| a.value.clone())
        .ok_or("SignatureMethod has no Algorithm")?;

    let signature_value = decode_base64_value(&doc.text_content(signed_value_node))?;

    let reference = doc
        .first_child_element_named(signed_info, Some(XMLDSIG_NS), "Reference")
        .ok_or("SignedInfo has no Reference")?;
    let reference_uri = doc
        .attrs_of(reference)
        .iter()
        .find(|a| a.local == "URI")
        .map(|a| a.value.clone())
        .ok_or("Reference has no URI")?;

    let mut transforms = Vec::new();
    if let Some(transforms_node) =
        doc.first_child_element_named(reference, Some(XMLDSIG_NS), "Transforms")
    {
        for transform in doc.child_elements(transforms_node) {
            if doc.element_local(transform) == "Transform" {
                let algorithm = doc
                    .attrs_of(transform)
                    .iter()
                    .find(|a| a.local == "Algorithm")
                    .map(|a| a.value.clone())
                    .ok_or("Transform has no Algorithm")?;
                transforms.push(Transform {
                    algorithm,
                    prefix_list: parse_prefix_list(doc, transform),
                });
            }
        }
    }

    let digest_node = doc
        .first_child_element_named(reference, Some(XMLDSIG_NS), "DigestMethod")
        .ok_or("Reference has no DigestMethod")?;
    let digest_method = doc
        .attrs_of(digest_node)
        .iter()
        .find(|a| a.local == "Algorithm")
        .map(|a| a.value.clone())
        .ok_or("DigestMethod has no Algorithm")?;
    let digest_value_node = doc
        .first_child_element_named(reference, Some(XMLDSIG_NS), "DigestValue")
        .ok_or("Reference has no DigestValue")?;
    let digest_value = decode_base64_value(&doc.text_content(digest_value_node))?;

    Ok(ParsedSignature {
        signature_node,
        signed_info,
        canonicalization,
        signature_method,
        signature_value,
        reference_uri,
        transforms,
        digest_method,
        digest_value,
    })
}

fn decode_base64_value(text: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| format!("base64: {e}"))
}

fn apply_digest(method: &str, data: &[u8]) -> Option<Vec<u8>> {
    match method {
        DIGEST_SHA1 => Some(Sha1::digest(data).to_vec()),
        DIGEST_SHA256 => Some(Sha256::digest(data).to_vec()),
        DIGEST_SHA384 => Some(Sha384::digest(data).to_vec()),
        DIGEST_SHA512 => Some(Sha512::digest(data).to_vec()),
        _ => None,
    }
}

fn verify_rsa(
    public_key: &RsaPublicKey,
    method: &str,
    message: &[u8],
    signature_bytes: &[u8],
) -> Option<()> {
    let signature = Signature::try_from(signature_bytes).ok()?;
    match method {
        RSA_SHA1 => VerifyingKey::<Sha1>::new(public_key.clone())
            .verify(message, &signature)
            .ok(),
        RSA_SHA256 => VerifyingKey::<Sha256>::new(public_key.clone())
            .verify(message, &signature)
            .ok(),
        RSA_SHA384 => VerifyingKey::<Sha384>::new(public_key.clone())
            .verify(message, &signature)
            .ok(),
        RSA_SHA512 => VerifyingKey::<Sha512>::new(public_key.clone())
            .verify(message, &signature)
            .ok(),
        _ => None,
    }
}

/// Port of `getVerifiedXml` from `@node-saml/node-saml` xml.ts: verify the
/// single signature that is a direct child of `element` and, on success,
/// return the canonical bytes of the element it signs (the signed reference).
/// `Ok(None)` means "no valid signature"; `Err` mirrors the hard-failure checks.
pub fn get_verified_xml(
    doc: &XmlDocument,
    element: usize,
    public_key: &RsaPublicKey,
) -> Result<Option<String>, String> {
    let signatures = doc
        .child_elements(element)
        .into_iter()
        .filter(|&child| {
            doc.element_local(child) == "Signature"
                && doc.element_namespace(child).as_deref() == Some(XMLDSIG_NS)
        })
        .collect::<Vec<_>>();
    if signatures.is_empty() {
        return Ok(None);
    }
    if signatures.len() > 1 {
        return Err("Too many signatures found for this element".into());
    }
    let signature_node = signatures[0];

    let transform_count = doc
        .descendants(signature_node)
        .iter()
        .filter(|&&node| doc.element_local(node) == "Transform")
        .count();
    if transform_count > 2 {
        return Err("Invalid signature, too many transforms".into());
    }

    let parsed = parse_signature(doc, signature_node)?;

    // Sanity checks from getVerifiedXml: exactly one reference to the parent.
    let reference_id = parsed
        .reference_uri
        .strip_prefix('#')
        .unwrap_or(&parsed.reference_uri)
        .to_string();
    if reference_id.is_empty() {
        return Err("signature reference uri not found".into());
    }
    if reference_id.contains('\'') || reference_id.contains('"') {
        return Err(
            "ref URI included quote character ' or \". Not a valid ID, and not allowed".into(),
        );
    }
    let referenced = doc.elements_with_id(&reference_id);
    if referenced.len() != 1 {
        return Err("Invalid signature: ID cannot refer to more than one element".into());
    }
    if referenced[0] != doc.parent_of(signature_node).unwrap_or(0) {
        return Err(
            "Invalid signature: Referenced node does not refer to it's parent element".into(),
        );
    }

    // Cryptographic verification: canonicalize SignedInfo, verify the value.
    let signed_info_canon = canonicalize(doc, parsed.signed_info, None, &parsed.canonicalization);
    if verify_rsa(
        public_key,
        &parsed.signature_method,
        signed_info_canon.as_bytes(),
        &parsed.signature_value,
    )
    .is_none()
    {
        return Ok(None);
    }

    // Reference digest over the referenced element after the transforms.
    let mut enveloped_removed = false;
    let mut c14n: Option<C14nConfig> = None;
    for transform in &parsed.transforms {
        match transform.algorithm.as_str() {
            ENVELOPED_NS => enveloped_removed = true,
            EC_INCLUSIVE_NS => {
                c14n = Some(C14nConfig {
                    exclusive: true,
                    with_comments: false,
                    prefix_list: transform.prefix_list.clone(),
                })
            }
            EC_INCLUSIVE_WITH_COMMENTS => {
                c14n = Some(C14nConfig {
                    exclusive: true,
                    with_comments: true,
                    prefix_list: transform.prefix_list.clone(),
                })
            }
            C14N10_NS => {
                c14n = Some(C14nConfig {
                    exclusive: false,
                    with_comments: false,
                    prefix_list: Vec::new(),
                })
            }
            C14N10_WITH_COMMENTS_NS => {
                c14n = Some(C14nConfig {
                    exclusive: false,
                    with_comments: true,
                    prefix_list: Vec::new(),
                })
            }
            _ => return Err("unsupported transform".into()),
        }
    }
    let c14n = c14n.unwrap_or(C14nConfig {
        exclusive: false,
        with_comments: false,
        prefix_list: Vec::new(),
    });
    let skip = if enveloped_removed {
        Some(signature_node)
    } else {
        None
    };
    let referenced_bytes = canonicalize(doc, referenced[0], skip, &c14n);
    let digest = apply_digest(&parsed.digest_method, referenced_bytes.as_bytes())
        .ok_or_else(|| "unsupported digest method".to_string())?;
    if digest != parsed.digest_value {
        return Ok(None);
    }
    Ok(Some(referenced_bytes))
}

/// Extract the RSA public key from an X.509 certificate (DER).
pub fn rsa_public_key_from_cert_der(der: &[u8]) -> Result<RsaPublicKey, String> {
    let (cert_tag, cert_content, _) = read_tlv(der)?;
    if cert_tag != 0x30 {
        return Err("expected DER SEQUENCE".into());
    }
    // tbsCertificate ::= [0] version?, serialNumber, signature, issuer,
    // validity, subject, subjectPublicKeyInfo, ...
    let (tbs_tag, tbs_content, _) = read_tlv(cert_content)?;
    if tbs_tag != 0x30 {
        return Err("expected tbsCertificate SEQUENCE".into());
    }
    let mut rest = tbs_content;
    if rest.first() == Some(&0xA0) {
        let (_, _, remainder) = read_tlv(rest)?;
        rest = remainder;
    }
    for _ in 0..5 {
        let (_, _, remainder) = read_tlv(rest)?;
        rest = remainder;
    }
    let (spki_tag, _, _) = read_tlv(rest)?;
    if spki_tag != 0x30 {
        return Err("certificate SubjectPublicKeyInfo not found".into());
    }
    // The SPKI TLV is the prefix of `rest` that the probe read consumed.
    let spki_full = &rest[..tlv_len(rest)?];
    RsaPublicKey::from_public_key_der(spki_full).map_err(|e| format!("public key: {e}"))
}

/// Complete length (header + content) of the TLV at the start of `input`.
fn tlv_len(input: &[u8]) -> Result<usize, String> {
    if input.len() < 2 {
        return Err("truncated DER".into());
    }
    let first = input[1] as usize;
    let (header, length) = if first < 0x80 {
        (2usize, first)
    } else {
        let count = first & 0x7F;
        if count == 0 || count > 4 || input.len() < 2 + count {
            return Err("unsupported DER length".into());
        }
        let mut length = 0usize;
        for byte in &input[2..2 + count] {
            length = (length << 8) | *byte as usize;
        }
        (2 + count, length)
    };
    header
        .checked_add(length)
        .filter(|len| *len <= input.len())
        .ok_or_else(|| "truncated DER".into())
}

fn read_tlv(input: &[u8]) -> Result<(u8, &[u8], &[u8]), String> {
    if input.len() < 2 {
        return Err("truncated DER".into());
    }
    let tag = input[0];
    let full_len = tlv_len(input)?;
    let first = input[1] as usize;
    let header = if first < 0x80 { 2 } else { 2 + (first & 0x7F) };
    Ok((tag, &input[header..full_len], &input[full_len..]))
}

/// Extract the RSA public key from a PEM certificate body.
pub fn parse_certificate_pem(pem: &str) -> Result<RsaPublicKey, String> {
    let body: String = pem
        .lines()
        .filter(|line| !line.trim_start().starts_with("-----"))
        .collect();
    let der = decode_base64_value(&body)?;
    rsa_public_key_from_cert_der(&der)
}

/// Port of `formatX509Certificate` from upstream `lib/auth/saml.js`: strip
/// any existing PEM armor and re-wrap the base64 payload in 64-column lines.
pub fn format_x509_certificate(cert: &str) -> String {
    let clean: String = cert
        .replace("-----BEGIN CERTIFICATE-----", "")
        .replace("-----END CERTIFICATE-----", "")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/' || *c == '=')
        .collect();
    if clean.is_empty() {
        return String::new();
    }
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in clean.chars().collect::<Vec<_>>().chunks(64) {
        out.extend(chunk);
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----");
    out
}
