/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! A safe wrapper for DOM nodes that prevents layout from mutating the DOM, from letting DOM nodes
//! escape, and from generally doing anything that it isn't supposed to. This is accomplished via
//! a simple whitelist of allowed operations, along with some lifetime magic to prevent nodes from
//! escaping.
//!
//! As a security wrapper is only as good as its whitelist, be careful when adding operations to
//! this list. The cardinal rules are:
//!
//! 1. Layout is not allowed to mutate the DOM.
//!
//! 2. Layout is not allowed to see anything with `LayoutJS` in the name, because it could hang
//!    onto these objects and cause use-after-free.
//!
//! When implementing wrapper functions, be careful that you do not touch the borrow flags, or you
//! will race and cause spurious task failure. (Note that I do not believe these races are
//! exploitable, but they'll result in brokenness nonetheless.)
//!
//! Rules of the road for this file:
//!
//! * Do not call any methods on DOM nodes without checking to see whether they use borrow flags.
//!
//!   o Instead of `get_attr()`, use `.get_attr_val_for_layout()`.
//!
//!   o Instead of `html_element_in_html_document()`, use
//!     `html_element_in_html_document_for_layout()`.

#![allow(unsafe_code)]

use canvas_traits::CanvasMsg;
use context::SharedLayoutContext;
use css::node_style::StyledNode;
use incremental::RestyleDamage;
use data::{LayoutDataFlags, LayoutDataWrapper, PrivateLayoutData};
use opaque_node::OpaqueNodeMethods;

use gfx::display_list::OpaqueNode;
use script::dom::attr::AttrValue;
use script::dom::bindings::codegen::InheritTypes::{CharacterDataCast, ElementCast};
use script::dom::bindings::codegen::InheritTypes::{HTMLIFrameElementCast, HTMLCanvasElementCast};
use script::dom::bindings::codegen::InheritTypes::{HTMLImageElementCast, HTMLInputElementCast};
use script::dom::bindings::codegen::InheritTypes::{HTMLTextAreaElementCast, NodeCast, TextCast};
use script::dom::bindings::js::LayoutJS;
use script::dom::characterdata::{CharacterDataTypeId, LayoutCharacterDataHelpers};
use script::dom::element::{Element, ElementTypeId};
use script::dom::element::{LayoutElementHelpers, RawLayoutElementHelpers};
use script::dom::htmlelement::HTMLElementTypeId;
use script::dom::htmlcanvaselement::LayoutHTMLCanvasElementHelpers;
use script::dom::htmlimageelement::LayoutHTMLImageElementHelpers;
use script::dom::htmlinputelement::{HTMLInputElement, LayoutHTMLInputElementHelpers};
use script::dom::htmltextareaelement::LayoutHTMLTextAreaElementHelpers;
use script::dom::node::{Node, NodeTypeId};
use script::dom::node::{LayoutNodeHelpers, RawLayoutNodeHelpers, SharedLayoutData};
use script::dom::node::{HAS_CHANGED, IS_DIRTY, HAS_DIRTY_SIBLINGS, HAS_DIRTY_DESCENDANTS};
use script::dom::text::Text;
use script::layout_interface::LayoutChan;
use smallvec::VecLike;
use msg::constellation_msg::{PipelineId, SubpageId};
use util::str::is_whitespace;
use std::borrow::ToOwned;
use std::cell::{Ref, RefMut};
use std::marker::PhantomData;
use std::mem;
use std::sync::mpsc::Sender;
use string_cache::{Atom, Namespace};
use style::computed_values::content::ContentItem;
use style::computed_values::{content, display, white_space};
use selectors::Node as SelectorsNode;
use selectors::matching::DeclarationBlock;
use selectors::parser::{NamespaceConstraint, AttrSelector};
use style::legacy::UnsignedIntegerAttribute;
use style::node::TElementAttributes;
use style::properties::{PropertyDeclaration, PropertyDeclarationBlock};
use url::Url;

/// A wrapper so that layout can access only the methods that it should have access to. Layout must
/// only ever see these and must never see instances of `LayoutJS`.
#[derive(Copy, Clone)]
pub struct LayoutNode<'a> {
    /// The wrapped node.
    node: LayoutJS<Node>,

    /// Being chained to a PhantomData prevents `LayoutNode`s from escaping.
    pub chain: PhantomData<&'a ()>,
}

impl<'a> PartialEq for LayoutNode<'a> {
    #[inline]
    fn eq(&self, other: &LayoutNode) -> bool {
        self.node == other.node
    }
}

impl<'ln> LayoutNode<'ln> {
    /// Creates a new layout node with the same lifetime as this layout node.
    pub unsafe fn new_with_this_lifetime(&self, node: &LayoutJS<Node>) -> LayoutNode<'ln> {
        LayoutNode {
            node: *node,
            chain: self.chain,
        }
    }

    /// Returns the type ID of this node.
    pub fn type_id(&self) -> NodeTypeId {
        unsafe {
            self.node.type_id_for_layout()
        }
    }

    pub fn dump(self) {
        self.dump_indent(0);
    }

    fn dump_indent(self, indent: u32) {
        let mut s = String::new();
        for _ in 0..indent {
            s.push_str("  ");
        }

        s.push_str(&self.debug_str());
        println!("{}", s);

        for kid in self.children() {
            kid.dump_indent(indent + 1);
        }
    }

    fn debug_str(self) -> String {
        format!("{:?}: changed={} dirty={} dirty_descendants={}",
                self.type_id(), self.has_changed(), self.is_dirty(), self.has_dirty_descendants())
    }

    pub fn flow_debug_id(self) -> usize {
        let layout_data_ref = self.borrow_layout_data();
        match *layout_data_ref {
            None => 0,
            Some(ref layout_data) => layout_data.data.flow_construction_result.debug_id()
        }
    }

    pub fn traverse_preorder(self) -> LayoutTreeIterator<'ln> {
        LayoutTreeIterator::new(self)
    }

    fn last_child(self) -> Option<LayoutNode<'ln>> {
        unsafe {
            self.get_jsmanaged().last_child_ref().map(|node| self.new_with_this_lifetime(&node))
        }
    }

    /// Returns an iterator over this node's children.
    pub fn children(self) -> LayoutNodeChildrenIterator<'ln> {
        LayoutNodeChildrenIterator {
            current: self.first_child(),
        }
    }

    pub fn rev_children(self) -> LayoutNodeReverseChildrenIterator<'ln> {
        LayoutNodeReverseChildrenIterator {
            current: self.last_child()
        }

    }

    /// Returns the interior of this node as a `LayoutJS`. This is highly unsafe for layout to
    /// call and as such is marked `unsafe`.
    unsafe fn get_jsmanaged<'a>(&'a self) -> &'a LayoutJS<Node> {
        &self.node
    }

    /// Converts self into an `OpaqueNode`.
    pub fn opaque(&self) -> OpaqueNode {
        OpaqueNodeMethods::from_jsmanaged(unsafe { self.get_jsmanaged() })
    }

    /// Resets layout data and styles for the node.
    ///
    /// FIXME(pcwalton): Do this as part of fragment building instead of in a traversal.
    pub fn initialize_layout_data(self, chan: LayoutChan) {
        let mut layout_data_ref = self.mutate_layout_data();
        match *layout_data_ref {
            None => {
                *layout_data_ref = Some(LayoutDataWrapper {
                    chan: Some(chan),
                    shared_data: SharedLayoutData { style: None },
                    data: box PrivateLayoutData::new(),
                });
            }
            Some(_) => {}
        }
    }

    pub fn has_children(self) -> bool {
        self.first_child().is_some()
    }

    /// While doing a reflow, the node at the root has no parent, as far as we're
    /// concerned. This method returns `None` at the reflow root.
    pub fn layout_parent_node(self, shared: &SharedLayoutContext) -> Option<LayoutNode<'ln>> {
        match shared.reflow_root {
            None => panic!("layout_parent_node(): This layout has no access to the DOM!"),
            Some(reflow_root) => {
                if self.opaque() == reflow_root {
                    None
                } else {
                    self.parent_node()
                }
            }
        }
    }

    pub fn debug_id(self) -> usize {
        self.opaque().to_untrusted_node_address().0 as usize
    }
}

impl<'ln> ::selectors::Node<LayoutElement<'ln>> for LayoutNode<'ln> {
    fn parent_node(&self) -> Option<LayoutNode<'ln>> {
        unsafe {
            self.node.parent_node_ref().map(|node| self.new_with_this_lifetime(&node))
        }
    }

    fn first_child(&self) -> Option<LayoutNode<'ln>> {
        unsafe {
            self.node.first_child_ref().map(|node| self.new_with_this_lifetime(&node))
        }
    }

    fn last_child(&self) -> Option<LayoutNode<'ln>> {
        unsafe {
            self.node.last_child_ref().map(|node| self.new_with_this_lifetime(&node))
        }
    }

    fn prev_sibling(&self) -> Option<LayoutNode<'ln>> {
        unsafe {
            self.node.prev_sibling_ref().map(|node| self.new_with_this_lifetime(&node))
        }
    }

    fn next_sibling(&self) -> Option<LayoutNode<'ln>> {
        unsafe {
            self.node.next_sibling_ref().map(|node| self.new_with_this_lifetime(&node))
        }
    }

    /// If this is an element, accesses the element data.
    #[inline]
    fn as_element(&self) -> Option<LayoutElement<'ln>> {
        ElementCast::to_layout_js(&self.node).map(|element| {
            LayoutElement {
                element: element,
                chain: self.chain,
            }
        })
    }

    fn is_document(&self) -> bool {
        match self.type_id() {
            NodeTypeId::Document(..) => true,
            _ => false
        }
    }
}

impl<'ln> LayoutNode<'ln> {
    pub fn has_changed(&self) -> bool {
        unsafe { self.node.get_flag(HAS_CHANGED) }
    }

    pub unsafe fn set_changed(&self, value: bool) {
        self.node.set_flag(HAS_CHANGED, value)
    }

    pub fn is_dirty(&self) -> bool {
        unsafe { self.node.get_flag(IS_DIRTY) }
    }

    pub unsafe fn set_dirty(&self, value: bool) {
        self.node.set_flag(IS_DIRTY, value)
    }

    pub unsafe fn set_dirty_siblings(&self, value: bool) {
        self.node.set_flag(HAS_DIRTY_SIBLINGS, value);
    }

    pub fn has_dirty_descendants(&self) -> bool {
        unsafe { self.node.get_flag(HAS_DIRTY_DESCENDANTS) }
    }

    pub unsafe fn set_dirty_descendants(&self, value: bool) {
        self.node.set_flag(HAS_DIRTY_DESCENDANTS, value)
    }

    /// Borrows the layout data without checks.
    #[inline(always)]
    pub unsafe fn borrow_layout_data_unchecked(&self) -> *const Option<LayoutDataWrapper> {
        mem::transmute(self.get_jsmanaged().layout_data_unchecked())
    }

    /// Borrows the layout data immutably. Fails on a conflicting borrow.
    #[inline(always)]
    pub fn borrow_layout_data<'a>(&'a self) -> Ref<'a,Option<LayoutDataWrapper>> {
        unsafe {
            mem::transmute(self.get_jsmanaged().layout_data())
        }
    }

    /// Borrows the layout data mutably. Fails on a conflicting borrow.
    #[inline(always)]
    pub fn mutate_layout_data<'a>(&'a self) -> RefMut<'a,Option<LayoutDataWrapper>> {
        unsafe {
            mem::transmute(self.get_jsmanaged().layout_data_mut())
        }
    }
}

pub struct LayoutNodeChildrenIterator<'a> {
    current: Option<LayoutNode<'a>>,
}

impl<'a> Iterator for LayoutNodeChildrenIterator<'a> {
    type Item = LayoutNode<'a>;
    fn next(&mut self) -> Option<LayoutNode<'a>> {
        let node = self.current;
        self.current = node.and_then(|node| node.next_sibling());
        node
    }
}

pub struct LayoutNodeReverseChildrenIterator<'a> {
    current: Option<LayoutNode<'a>>,
}

impl<'a> Iterator for LayoutNodeReverseChildrenIterator<'a> {
    type Item = LayoutNode<'a>;
    fn next(&mut self) -> Option<LayoutNode<'a>> {
        let node = self.current;
        self.current = node.and_then(|node| node.prev_sibling());
        node
    }
}

pub struct LayoutTreeIterator<'a> {
    stack: Vec<LayoutNode<'a>>,
}

impl<'a> LayoutTreeIterator<'a> {
    fn new(root: LayoutNode<'a>) -> LayoutTreeIterator<'a> {
        let mut stack = vec!();
        stack.push(root);
        LayoutTreeIterator {
            stack: stack
        }
    }
}

impl<'a> Iterator for LayoutTreeIterator<'a> {
    type Item = LayoutNode<'a>;
    fn next(&mut self) -> Option<LayoutNode<'a>> {
        let ret = self.stack.pop();
        ret.map(|node| self.stack.extend(node.rev_children()));
        ret
    }
}

/// A wrapper around elements that ensures layout can only ever access safe properties.
#[derive(Copy, Clone)]
pub struct LayoutElement<'le> {
    element: LayoutJS<Element>,
    chain: PhantomData<&'le ()>,
}

impl<'le> LayoutElement<'le> {
    pub fn style_attribute(&self) -> &'le Option<PropertyDeclarationBlock> {
        use script::dom::element::ElementHelpers;
        let style: &Option<PropertyDeclarationBlock> = unsafe {
            &*(*self.element.unsafe_get()).style_attribute().borrow_for_layout()
        };
        style
    }
}

impl<'le> ::selectors::Element for LayoutElement<'le> {
    type Node = LayoutNode<'le>;

    #[inline]
    fn as_node(&self) -> LayoutNode<'le> {
        LayoutNode {
            node: NodeCast::from_layout_js(&self.element),
            chain: PhantomData,
        }
    }

    #[inline]
    fn get_local_name<'a>(&'a self) -> &'a Atom {
        unsafe {
            (*self.element.unsafe_get()).local_name()
        }
    }

    #[inline]
    fn get_namespace<'a>(&'a self) -> &'a Namespace {
        use script::dom::element::ElementHelpers;
        unsafe {
            (*self.element.unsafe_get()).namespace()
        }
    }

    fn is_link(&self) -> bool {
        // FIXME: This is HTML only.
        let node = NodeCast::from_layout_js(&self.element);
        match unsafe { (*node.unsafe_get()).type_id_for_layout() } {
            // https://html.spec.whatwg.org/multipage/#selector-link
            NodeTypeId::Element(ElementTypeId::HTMLElement(HTMLElementTypeId::HTMLAnchorElement)) |
            NodeTypeId::Element(ElementTypeId::HTMLElement(HTMLElementTypeId::HTMLAreaElement)) |
            NodeTypeId::Element(ElementTypeId::HTMLElement(HTMLElementTypeId::HTMLLinkElement)) => {
                unsafe {
                    (*self.element.unsafe_get()).get_attr_val_for_layout(&ns!(""), &atom!("href")).is_some()
                }
            }
            _ => false,
        }
    }

    #[inline]
    fn is_unvisited_link(&self) -> bool {
        self.is_link()
    }

    #[inline]
    fn is_visited_link(&self) -> bool {
        false
    }

    #[inline]
    fn get_hover_state(&self) -> bool {
        let node = NodeCast::from_layout_js(&self.element);
        unsafe {
            (*node.unsafe_get()).get_hover_state_for_layout()
        }
    }

    #[inline]
    fn get_focus_state(&self) -> bool {
        let node = NodeCast::from_layout_js(&self.element);
        unsafe {
            (*node.unsafe_get()).get_focus_state_for_layout()
        }
    }

    #[inline]
    fn get_id(&self) -> Option<Atom> {
        unsafe {
            (*self.element.unsafe_get()).get_attr_atom_for_layout(&ns!(""), &atom!("id"))
        }
    }

    #[inline]
    fn get_disabled_state(&self) -> bool {
        let node = NodeCast::from_layout_js(&self.element);
        unsafe {
            (*node.unsafe_get()).get_disabled_state_for_layout()
        }
    }

    #[inline]
    fn get_enabled_state(&self) -> bool {
        let node = NodeCast::from_layout_js(&self.element);
        unsafe {
            (*node.unsafe_get()).get_enabled_state_for_layout()
        }
    }

    #[inline]
    fn get_checked_state(&self) -> bool {
        unsafe {
            (*self.element.unsafe_get()).get_checked_state_for_layout()
        }
    }

    #[inline]
    fn get_indeterminate_state(&self) -> bool {
        unsafe {
            (*self.element.unsafe_get()).get_indeterminate_state_for_layout()
        }
    }

    #[inline]
    fn has_class(&self, name: &Atom) -> bool {
        unsafe {
            (*self.element.unsafe_get()).has_class_for_layout(name)
        }
    }

    #[inline(always)]
    fn each_class<F>(&self, mut callback: F) where F: FnMut(&Atom) {
        unsafe {
            match (*self.element.unsafe_get()).get_classes_for_layout() {
                None => {}
                Some(ref classes) => {
                    for class in classes.iter() {
                        callback(class)
                    }
                }
            }
        }
    }

    #[inline]
    fn has_servo_nonzero_border(&self) -> bool {
        unsafe {
            match (*self.element.unsafe_get()).get_attr_for_layout(&ns!(""), &atom!("border")) {
                None | Some(&AttrValue::UInt(_, 0)) => false,
                _ => true,
            }
        }
    }

    fn match_attr<F>(&self, attr: &AttrSelector, test: F) -> bool where F: Fn(&str) -> bool {
        let name = if self.is_html_element_in_html_document() {
            &attr.lower_name
        } else {
            &attr.name
        };
        match attr.namespace {
            NamespaceConstraint::Specific(ref ns) => {
                self.get_attr(ns, name).map_or(false, |attr| test(attr))
            },
            NamespaceConstraint::Any => {
                self.get_attrs(name).iter().any(|attr| test(*attr))
            }
        }
    }

    fn is_html_element_in_html_document(&self) -> bool {
        unsafe {
            self.element.html_element_in_html_document_for_layout()
        }
    }
}

impl<'le> TElementAttributes for LayoutElement<'le> {
    fn synthesize_presentational_hints_for_legacy_attributes<V>(&self, hints: &mut V)
        where V: VecLike<DeclarationBlock<Vec<PropertyDeclaration>>>
    {
        unsafe {
            (*self.element.unsafe_get()).synthesize_presentational_hints_for_legacy_attributes(hints);
        }
    }

    fn get_unsigned_integer_attribute(&self, attribute: UnsignedIntegerAttribute) -> Option<u32> {
        unsafe {
            (*self.element.unsafe_get()).get_unsigned_integer_attribute_for_layout(attribute)
        }
    }

    #[inline]
    fn get_attr<'a>(&'a self, namespace: &Namespace, name: &Atom) -> Option<&'a str> {
        unsafe {
            (*self.element.unsafe_get()).get_attr_val_for_layout(namespace, name)
        }
    }

    #[inline]
    fn get_attrs<'a>(&'a self, name: &Atom) -> Vec<&'a str> {
        unsafe {
            (*self.element.unsafe_get()).get_attr_vals_for_layout(name)
        }
    }
}

#[derive(Copy, PartialEq, Clone)]
pub enum PseudoElementType {
    Normal,
    Before(display::T),
    After(display::T),
}

impl PseudoElementType {
    pub fn is_before(&self) -> bool {
        match *self {
            PseudoElementType::Before(_) => true,
            _ => false,
        }
    }

    pub fn is_after(&self) -> bool {
        match *self {
            PseudoElementType::After(_) => true,
            _ => false,
        }
    }
}

/// A thread-safe version of `LayoutNode`, used during flow construction. This type of layout
/// node does not allow any parents or siblings of nodes to be accessed, to avoid races.
#[derive(Copy, Clone)]
pub struct ThreadSafeLayoutNode<'ln> {
    /// The wrapped node.
    node: LayoutNode<'ln>,

    pseudo: PseudoElementType,
}

impl<'ln> ThreadSafeLayoutNode<'ln> {
    /// Creates a new layout node with the same lifetime as this layout node.
    pub unsafe fn new_with_this_lifetime(&self, node: &LayoutJS<Node>) -> ThreadSafeLayoutNode<'ln> {
        ThreadSafeLayoutNode {
            node: self.node.new_with_this_lifetime(node),
            pseudo: PseudoElementType::Normal,
        }
    }

    /// Creates a new `ThreadSafeLayoutNode` from the given `LayoutNode`.
    pub fn new<'a>(node: &LayoutNode<'a>) -> ThreadSafeLayoutNode<'a> {
        ThreadSafeLayoutNode {
            node: node.clone(),
            pseudo: PseudoElementType::Normal,
        }
    }

    /// Creates a new `ThreadSafeLayoutNode` for the same `LayoutNode`
    /// with a different pseudo-element type.
    fn with_pseudo(&self, pseudo: PseudoElementType) -> ThreadSafeLayoutNode<'ln> {
        ThreadSafeLayoutNode {
            node: self.node.clone(),
            pseudo: pseudo,
        }
    }

    /// Returns the interior of this node as a `LayoutJS`. This is highly unsafe for layout to
    /// call and as such is marked `unsafe`.
    unsafe fn get_jsmanaged<'a>(&'a self) -> &'a LayoutJS<Node> {
        self.node.get_jsmanaged()
    }

    /// Converts self into an `OpaqueNode`.
    pub fn opaque(&self) -> OpaqueNode {
        OpaqueNodeMethods::from_jsmanaged(unsafe { self.get_jsmanaged() })
    }

    /// Returns the type ID of this node.
    /// Returns `None` if this is a pseudo-element; otherwise, returns `Some`.
    pub fn type_id(&self) -> Option<NodeTypeId> {
        if self.pseudo != PseudoElementType::Normal {
            return None
        }

        Some(self.node.type_id())
    }

    pub fn debug_id(self) -> usize {
        self.node.debug_id()
    }

    pub fn flow_debug_id(self) -> usize {
        self.node.flow_debug_id()
    }

    fn first_child(&self) -> Option<ThreadSafeLayoutNode<'ln>> {
        if self.pseudo != PseudoElementType::Normal {
            return None
        }

        if self.has_before_pseudo() {
            return Some(self.with_pseudo(PseudoElementType::Before(self.get_before_display())));
        }

        unsafe {
            self.get_jsmanaged().first_child_ref().map(|node| self.new_with_this_lifetime(&node))
        }
    }

    /// Returns the next sibling of this node. Unsafe and private because this can lead to races.
    unsafe fn next_sibling(&self) -> Option<ThreadSafeLayoutNode<'ln>> {
        if self.pseudo.is_before() {
            return self.get_jsmanaged().first_child_ref().map(|node| self.new_with_this_lifetime(&node))
        }

        self.get_jsmanaged().next_sibling_ref().map(|node| self.new_with_this_lifetime(&node))
    }

    /// Returns an iterator over this node's children.
    pub fn children(&self) -> ThreadSafeLayoutNodeChildrenIterator<'ln> {
        ThreadSafeLayoutNodeChildrenIterator {
            current_node: self.first_child(),
            parent_node: Some(self.clone()),
        }
    }

    /// If this is an element, accesses the element data. Fails if this is not an element node.
    #[inline]
    pub fn as_element(&self) -> ThreadSafeLayoutElement<'ln> {
        unsafe {
            let element = match ElementCast::to_layout_js(self.get_jsmanaged()) {
                Some(e) => e.unsafe_get(),
                None => panic!("not an element")
            };
            // FIXME(pcwalton): Workaround until Rust gets multiple lifetime parameters on
            // implementations.
            ThreadSafeLayoutElement {
                element: &*element,
            }
        }
    }

    #[inline]
    pub fn get_pseudo_element_type(&self) -> PseudoElementType {
        self.pseudo
    }

    #[inline]
    pub fn get_normal_display(&self) -> display::T {
        let mut layout_data_ref = self.mutate_layout_data();
        let node_layout_data_wrapper = layout_data_ref.as_mut().unwrap();
        let style = node_layout_data_wrapper.shared_data.style.as_ref().unwrap();
        style.get_box().display
    }

    #[inline]
    pub fn get_before_display(&self) -> display::T {
        let mut layout_data_ref = self.mutate_layout_data();
        let node_layout_data_wrapper = layout_data_ref.as_mut().unwrap();
        let style = node_layout_data_wrapper.data.before_style.as_ref().unwrap();
        style.get_box().display
    }

    #[inline]
    pub fn get_after_display(&self) -> display::T {
        let mut layout_data_ref = self.mutate_layout_data();
        let node_layout_data_wrapper = layout_data_ref.as_mut().unwrap();
        let style = node_layout_data_wrapper.data.after_style.as_ref().unwrap();
        style.get_box().display
    }

    #[inline]
    pub fn has_before_pseudo(&self) -> bool {
        let layout_data_wrapper = self.borrow_layout_data();
        let layout_data_wrapper_ref = layout_data_wrapper.as_ref().unwrap();
        layout_data_wrapper_ref.data.before_style.is_some()
    }

    #[inline]
    pub fn has_after_pseudo(&self) -> bool {
        let layout_data_wrapper = self.borrow_layout_data();
        let layout_data_wrapper_ref = layout_data_wrapper.as_ref().unwrap();
        layout_data_wrapper_ref.data.after_style.is_some()
    }

    /// Borrows the layout data without checking.
    #[inline(always)]
    fn borrow_layout_data_unchecked<'a>(&'a self) -> *const Option<LayoutDataWrapper> {
        unsafe {
            self.node.borrow_layout_data_unchecked()
        }
    }

    /// Borrows the layout data immutably. Fails on a conflicting borrow.
    ///
    /// TODO(pcwalton): Make this private. It will let us avoid borrow flag checks in some cases.
    #[inline(always)]
    pub fn borrow_layout_data<'a>(&'a self) -> Ref<'a,Option<LayoutDataWrapper>> {
        self.node.borrow_layout_data()
    }

    /// Borrows the layout data mutably. Fails on a conflicting borrow.
    ///
    /// TODO(pcwalton): Make this private. It will let us avoid borrow flag checks in some cases.
    #[inline(always)]
    pub fn mutate_layout_data<'a>(&'a self) -> RefMut<'a,Option<LayoutDataWrapper>> {
        self.node.mutate_layout_data()
    }

    /// Traverses the tree in postorder.
    ///
    /// TODO(pcwalton): Offer a parallel version with a compatible API.
    pub fn traverse_postorder_mut<T:PostorderNodeMutTraversal>(&mut self, traversal: &mut T)
                                  -> bool {
        if traversal.should_prune(self) {
            return true
        }

        let mut opt_kid = self.first_child();
        while let Some(mut kid) = opt_kid {
            if !kid.traverse_postorder_mut(traversal) {
                return false
            }
            unsafe {
                opt_kid = kid.next_sibling()
            }
        }

        traversal.process(self)
    }

    pub fn is_ignorable_whitespace(&self) -> bool {
        unsafe {
            let text: LayoutJS<Text> = match TextCast::to_layout_js(self.get_jsmanaged()) {
                Some(text) => text,
                None => return false
            };

            if !is_whitespace(CharacterDataCast::from_layout_js(&text).data_for_layout()) {
                return false
            }

            // NB: See the rules for `white-space` here:
            //
            //    http://www.w3.org/TR/CSS21/text.html#propdef-white-space
            //
            // If you implement other values for this property, you will almost certainly
            // want to update this check.
            match self.style().get_inheritedtext().white_space {
                white_space::T::normal => true,
                _ => false,
            }
        }
    }

    pub fn get_input_value(&self) -> String {
        unsafe {
            let input: Option<LayoutJS<HTMLInputElement>> = HTMLInputElementCast::to_layout_js(self.get_jsmanaged());
            match input {
                Some(input) => input.get_value_for_layout(),
                None => panic!("not an input element!")
            }
        }
    }

    pub fn get_input_size(&self) -> u32 {
        unsafe {
            match HTMLInputElementCast::to_layout_js(self.get_jsmanaged()) {
                Some(input) => input.get_size_for_layout(),
                None => panic!("not an input element!")
            }
        }
    }

    pub fn get_unsigned_integer_attribute(self, attribute: UnsignedIntegerAttribute)
                                          -> Option<u32> {
        unsafe {
            let elem: Option<LayoutJS<Element>> = ElementCast::to_layout_js(self.get_jsmanaged());
            match elem {
                Some(element) => {
                    (*element.unsafe_get()).get_unsigned_integer_attribute_for_layout(attribute)
                }
                None => panic!("not an element!")
            }
        }
    }

    /// Get the description of how to account for recent style changes.
    /// This is a simple bitfield and fine to copy by value.
    pub fn restyle_damage(self) -> RestyleDamage {
        let layout_data_ref = self.borrow_layout_data();
        layout_data_ref.as_ref().unwrap().data.restyle_damage
    }

    /// Set the restyle damage field.
    pub fn set_restyle_damage(self, damage: RestyleDamage) {
        let mut layout_data_ref = self.mutate_layout_data();
        match &mut *layout_data_ref {
            &mut Some(ref mut layout_data) => layout_data.data.restyle_damage = damage,
            _ => panic!("no layout data for this node"),
        }
    }

    /// Returns the layout data flags for this node.
    pub fn flags(self) -> LayoutDataFlags {
        unsafe {
            match *self.borrow_layout_data_unchecked() {
                None => panic!(),
                Some(ref layout_data) => layout_data.data.flags,
            }
        }
    }

    /// Adds the given flags to this node.
    pub fn insert_flags(self, new_flags: LayoutDataFlags) {
        let mut layout_data_ref = self.mutate_layout_data();
        match &mut *layout_data_ref {
            &mut Some(ref mut layout_data) => layout_data.data.flags.insert(new_flags),
            _ => panic!("no layout data for this node"),
        }
    }

    /// Removes the given flags from this node.
    pub fn remove_flags(self, flags: LayoutDataFlags) {
        let mut layout_data_ref = self.mutate_layout_data();
        match &mut *layout_data_ref {
            &mut Some(ref mut layout_data) => layout_data.data.flags.remove(flags),
            _ => panic!("no layout data for this node"),
        }
    }

    /// Returns true if this node contributes content. This is used in the implementation of
    /// `empty_cells` per CSS 2.1 § 17.6.1.1.
    pub fn is_content(&self) -> bool {
        match self.type_id() {
            Some(NodeTypeId::Element(..)) | Some(NodeTypeId::CharacterData(CharacterDataTypeId::Text(..))) => true,
            _ => false
        }
    }

    /// If this is a text node, generated content, or a form element, copies out
    /// its content. Otherwise, panics.
    ///
    /// FIXME(pcwalton): This might have too much copying and/or allocation. Profile this.
    pub fn text_content(&self) -> Vec<ContentItem> {
        if self.pseudo != PseudoElementType::Normal {
            let layout_data_ref = self.borrow_layout_data();
            let data = &layout_data_ref.as_ref().unwrap().data;

            let style = if self.pseudo.is_before() {
                &data.before_style
            } else {
                &data.after_style
            };
            return match style.as_ref().unwrap().get_box().content {
                content::T::Content(ref value) if !value.is_empty() => (*value).clone(),
                _ => vec![],
            };
        }

        let this = unsafe { self.get_jsmanaged() };
        let text = TextCast::to_layout_js(this);
        if let Some(text) = text {
            let data = unsafe {
                CharacterDataCast::from_layout_js(&text).data_for_layout().to_owned()
            };
            return vec![ContentItem::String(data)];
        }
        let input = HTMLInputElementCast::to_layout_js(this);
        if let Some(input) = input {
            let data = unsafe { input.get_value_for_layout() };
            return vec![ContentItem::String(data)];
        }
        let area = HTMLTextAreaElementCast::to_layout_js(this);
        if let Some(area) = area {
            let data = unsafe { area.get_value_for_layout() };
            return vec![ContentItem::String(data)];
        }

        panic!("not text!")
    }

    /// If this is an image element, returns its URL. If this is not an image element, fails.
    ///
    /// FIXME(pcwalton): Don't copy URLs.
    pub fn image_url(&self) -> Option<Url> {
        unsafe {
            HTMLImageElementCast::to_layout_js(self.get_jsmanaged())
                .expect("not an image!")
                .image_url()
        }
    }

    pub fn renderer(&self) -> Option<Sender<CanvasMsg>> {
        unsafe {
            let canvas_element = HTMLCanvasElementCast::to_layout_js(self.get_jsmanaged());
            canvas_element.and_then(|elem| elem.get_renderer())
        }
    }

    pub fn canvas_width(&self) -> u32 {
        unsafe {
            let canvas_element = HTMLCanvasElementCast::to_layout_js(self.get_jsmanaged());
            canvas_element.unwrap().get_canvas_width()
        }
    }

    pub fn canvas_height(&self) -> u32 {
        unsafe {
            let canvas_element = HTMLCanvasElementCast::to_layout_js(self.get_jsmanaged());
            canvas_element.unwrap().get_canvas_height()
        }
    }

    /// If this node is an iframe element, returns its pipeline and subpage IDs. If this node is
    /// not an iframe element, fails.
    pub fn iframe_pipeline_and_subpage_ids(&self) -> (PipelineId, SubpageId) {
        unsafe {
            let iframe_element = HTMLIFrameElementCast::to_layout_js(self.get_jsmanaged())
                .expect("not an iframe element!");
            ((*iframe_element.unsafe_get()).containing_page_pipeline_id().unwrap(),
             (*iframe_element.unsafe_get()).subpage_id().unwrap())
        }
    }
}

pub struct ThreadSafeLayoutNodeChildrenIterator<'a> {
    current_node: Option<ThreadSafeLayoutNode<'a>>,
    parent_node: Option<ThreadSafeLayoutNode<'a>>,
}

impl<'a> Iterator for ThreadSafeLayoutNodeChildrenIterator<'a> {
    type Item = ThreadSafeLayoutNode<'a>;
    fn next(&mut self) -> Option<ThreadSafeLayoutNode<'a>> {
        let node = self.current_node.clone();

        match node {
            Some(ref node) => {
                if node.pseudo.is_after() {
                    return None
                }

                match self.parent_node {
                    Some(ref parent_node) => {
                        self.current_node = if parent_node.pseudo == PseudoElementType::Normal {
                            self.current_node.clone().and_then(|node| {
                                unsafe {
                                    node.next_sibling()
                                }
                            })
                        } else {
                            None
                        };
                    }
                    None => {}
                }
            }
            None => {
                match self.parent_node {
                    Some(ref parent_node) => {
                        if parent_node.has_after_pseudo() {
                            let pseudo_after_node = if parent_node.pseudo == PseudoElementType::Normal {
                                let pseudo = PseudoElementType::After(parent_node.get_after_display());
                                Some(parent_node.with_pseudo(pseudo))
                            } else {
                                None
                            };
                            self.current_node = pseudo_after_node;
                            return self.current_node.clone()
                        }
                   }
                   None => {}
                }
            }
        }

        node
    }
}

/// A wrapper around elements that ensures layout can only ever access safe properties and cannot
/// race on elements.
pub struct ThreadSafeLayoutElement<'le> {
    element: &'le Element,
}

impl<'le> ThreadSafeLayoutElement<'le> {
    #[inline]
    pub fn get_attr(&self, namespace: &Namespace, name: &Atom) -> Option<&'le str> {
        unsafe {
            self.element.get_attr_val_for_layout(namespace, name)
        }
    }
}

/// A bottom-up, parallelizable traversal.
pub trait PostorderNodeMutTraversal {
    /// The operation to perform. Return true to continue or false to stop.
    fn process<'a>(&'a mut self, node: &ThreadSafeLayoutNode<'a>) -> bool;

    /// Returns true if this node should be pruned. If this returns true, we skip the operation
    /// entirely and do not process any descendant nodes. This is called *before* child nodes are
    /// visited. The default implementation never prunes any nodes.
    fn should_prune<'a>(&'a self, _node: &ThreadSafeLayoutNode<'a>) -> bool {
        false
    }
}

/// Opaque type stored in type-unsafe work queues for parallel layout.
/// Must be transmutable to and from LayoutNode.
pub type UnsafeLayoutNode = (usize, usize);

pub fn layout_node_to_unsafe_layout_node(node: &LayoutNode) -> UnsafeLayoutNode {
    unsafe {
        let ptr: usize = mem::transmute_copy(node);
        (ptr, 0)
    }
}

// FIXME(#3044): This should be updated to use a real lifetime instead of
// faking one.
pub unsafe fn layout_node_from_unsafe_layout_node(node: &UnsafeLayoutNode) -> LayoutNode<'static> {
    let (node, _) = *node;
    mem::transmute(node)
}

/// A top-down traversal.
pub trait PreorderDomTraversal {
    /// The operation to perform. Return true to continue or false to stop.
    fn process(&self, node: LayoutNode);
}

/// A bottom-up traversal, with a optional in-order pass.
pub trait PostorderDomTraversal {
    /// The operation to perform. Return true to continue or false to stop.
    fn process(&self, node: LayoutNode);
}
