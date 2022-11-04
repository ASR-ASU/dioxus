use std::task::Context;

use futures_util::task::noop_waker_ref;
use futures_util::{pin_mut, Future};

use crate::factory::RenderReturn;
use crate::mutations::Mutation;
use crate::mutations::Mutation::*;
use crate::nodes::VNode;
use crate::nodes::{DynamicNode, TemplateNode};
use crate::virtualdom::VirtualDom;
use crate::{AttributeValue, Element, ElementId, TemplateAttribute};

impl VirtualDom {
    /// Create this template and write its mutations
    pub fn create<'a>(
        &mut self,
        mutations: &mut Vec<Mutation<'a>>,
        template: &'a VNode<'a>,
    ) -> usize {
        // The best renderers will have templates prehydrated
        // Just in case, let's create the template using instructions anyways
        if !self.templates.contains_key(&template.template.id) {
            for node in template.template.roots {
                self.create_static_node(mutations, template, node);
            }

            mutations.push(SaveTemplate {
                name: template.template.id,
                m: template.template.roots.len(),
            });

            self.templates
                .insert(template.template.id, template.template.clone());
        }

        // Walk the roots backwards, creating nodes and assigning IDs
        // todo: adjust dynamic nodes to be in the order of roots and then leaves (ie BFS)
        let mut dynamic_attrs = template.template.attr_paths.iter().enumerate().peekable();
        let mut dynamic_nodes = template.template.node_paths.iter().enumerate().peekable();

        let mut on_stack = 0;
        for (root_idx, root) in template.template.roots.iter().enumerate() {
            on_stack += match root {
                TemplateNode::Element { .. } | TemplateNode::Text(_) => {
                    mutations.push(LoadTemplate {
                        name: template.template.id,
                        index: root_idx,
                    });

                    1
                }

                TemplateNode::Dynamic(id) => {
                    self.create_dynamic_node(mutations, template, &template.dynamic_nodes[*id], *id)
                }

                TemplateNode::DynamicText { .. } => 1,
            };

            let mut cur_route = None;

            // we're on top of a node that has a dynamic attribute for a descendant
            // Set that attribute now before the stack gets in a weird state
            while let Some((idx, path)) = dynamic_attrs.next_if(|(_, p)| p[0] == root_idx as u8) {
                let attr = &template.dynamic_attrs[idx];

                if cur_route.is_none() {
                    cur_route = Some((self.next_element(template), &path[1..]));
                }

                // Attach all the elementIDs to the nodes with dynamic content
                let (id, path) = cur_route.unwrap();

                mutations.push(AssignId { path, id });
                attr.mounted_element.set(id);

                match attr.value {
                    AttributeValue::Text(value) => {
                        mutations.push(SetAttribute {
                            name: attr.name,
                            value,
                            id,
                        });
                    }

                    AttributeValue::Listener(_) => {
                        //
                    }

                    AttributeValue::Float(_) => todo!(),
                    AttributeValue::Int(_) => todo!(),
                    AttributeValue::Bool(_) => todo!(),
                    AttributeValue::Any(_) => todo!(),

                    // Optional attributes
                    AttributeValue::None => todo!(),
                }
            }

            // We're on top of a node that has a dynamic child for a descendant
            while let Some((idx, path)) = dynamic_nodes.next_if(|(_, p)| p[0] == root_idx as u8) {
                let node = &template.dynamic_nodes[idx];
                let m = self.create_dynamic_node(mutations, template, node, idx);
                if m > 0 {
                    mutations.push(ReplacePlaceholder {
                        m,
                        path: &path[1..],
                    });
                }
            }
        }

        on_stack
    }

    pub fn create_static_node<'a>(
        &mut self,
        mutations: &mut Vec<Mutation<'a>>,
        template: &'a VNode<'a>,
        node: &'a TemplateNode<'static>,
    ) {
        match *node {
            // Todo: create the children's template
            TemplateNode::Dynamic(_) => mutations.push(CreatePlaceholder { id: ElementId(0) }),
            TemplateNode::Text(value) => mutations.push(CreateText { value }),
            TemplateNode::DynamicText { .. } => mutations.push(CreateText {
                value: "placeholder",
            }),
            TemplateNode::Element {
                attrs,
                children,
                namespace,
                tag,
            } => {
                let id = self.next_element(template);

                mutations.push(CreateElement {
                    name: tag,
                    namespace,
                    id,
                });

                mutations.extend(attrs.into_iter().filter_map(|attr| match attr {
                    TemplateAttribute::Static { name, value, .. } => {
                        Some(SetAttribute { name, value, id })
                    }
                    _ => None,
                }));

                children
                    .into_iter()
                    .for_each(|child| self.create_static_node(mutations, template, child));

                mutations.push(AppendChildren { m: children.len() })
            }
        }
    }

    pub fn create_dynamic_node<'a>(
        &mut self,
        mutations: &mut Vec<Mutation<'a>>,
        template: &'a VNode<'a>,
        node: &'a DynamicNode<'a>,
        idx: usize,
    ) -> usize {
        match &node {
            DynamicNode::Text { id, value } => {
                let new_id = self.next_element(template);
                id.set(new_id);
                mutations.push(HydrateText {
                    id: new_id,
                    path: &template.template.node_paths[idx][1..],
                    value,
                });

                1
            }

            DynamicNode::Component { props, .. } => {
                let id = self.new_scope(unsafe { std::mem::transmute(props.get()) });

                let render_ret = self.run_scope(id);

                // shut up about lifetimes please, I know what I'm doing
                let render_ret: &mut RenderReturn = unsafe { std::mem::transmute(render_ret) };

                match render_ret {
                    RenderReturn::Sync(Some(template)) => {
                        self.scope_stack.push(id);
                        let created = self.create(mutations, template);
                        self.scope_stack.pop();
                        created
                    }
                    RenderReturn::Sync(None) => todo!("nodes that return nothing"),
                    RenderReturn::Async(fut) => {
                        use futures_util::FutureExt;

                        // Poll the suspense node once to see if we can get any nodes from it
                        let mut cx = Context::from_waker(&noop_waker_ref());
                        let res = fut.poll_unpin(&mut cx);

                        match res {
                            std::task::Poll::Ready(Some(val)) => {
                                let scope = self.get_scope(id).unwrap();
                                let ready = &*scope.bump().alloc(val);
                                let ready = unsafe { std::mem::transmute(ready) };

                                self.scope_stack.push(id);
                                let created = self.create(mutations, ready);
                                self.scope_stack.pop();
                                created
                            }
                            std::task::Poll::Ready(None) => {
                                todo!("Pending suspense")
                            }
                            std::task::Poll::Pending => {
                                let new_id = self.next_element(template);
                                // id.set(new_id);
                                mutations.push(AssignId {
                                    id: new_id,
                                    path: &template.template.node_paths[idx][1..],
                                });

                                0
                            }
                        }
                    }
                }
            }

            DynamicNode::Fragment(children) => children
                .iter()
                .fold(0, |acc, child| acc + self.create(mutations, child)),

            DynamicNode::Placeholder(_) => {
                let id = self.next_element(template);
                mutations.push(CreatePlaceholder { id });
                1
            }
        }
    }
}
