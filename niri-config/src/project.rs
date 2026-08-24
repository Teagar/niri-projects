use knuffel::errors::DecodeError;

use crate::{Color, SpawnAtStartup};

#[derive(knuffel::Decode, Debug, Clone, PartialEq)]
pub struct ProjectWorkspaceConfig {
    #[knuffel(argument)]
    pub name: String,
    #[knuffel(children)]
    pub spawn_at_startup: Vec<SpawnAtStartup>,
}

#[derive(knuffel::Decode, Debug, Clone, Copy, PartialEq)]
pub struct ProjectOverviewBorder {
    #[knuffel(child)]
    pub color: Color,
}

#[derive(knuffel::Decode, Debug, Clone, PartialEq)]
pub struct ProjectConfig {
    #[knuffel(argument)]
    pub name: String,
    #[knuffel(child)]
    pub keep_open: bool,
    #[knuffel(child)]
    pub overview_border: Option<ProjectOverviewBorder>,
    #[knuffel(children)]
    pub workspaces: Vec<ProjectWorkspaceConfig>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectsConfig(pub Vec<ProjectConfig>);

impl<S> knuffel::Decode<S> for ProjectsConfig
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let mut projects = Vec::new();
        for child in node.children() {
            let name = &**child.node_name;
            if name == "project" {
                let project = ProjectConfig::decode_node(child, ctx)?;
                projects.push(project);
            } else {
                ctx.emit_error(DecodeError::unexpected(
                    child,
                    "node",
                    format!("unexpected node `{name}` inside `projects`"),
                ));
            }
        }
        Ok(Self(projects))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(knuffel::Decode, Debug)]
    struct Wrapper {
        #[knuffel(child)]
        project: ProjectConfig,
    }

    fn parse_project(text: &str) -> ProjectConfig {
        let wrapper: Wrapper = knuffel::parse("test.kdl", text).unwrap();
        wrapper.project
    }

    #[test]
    fn parse_project_with_workspace() {
        let text = r#"
            project "neovim" {
                workspace "editor" {
                    spawn-at-startup "foot" "-e" "nvim"
                }
            }
        "#;
        let config = parse_project(text);
        assert_eq!(config.name, "neovim");
        assert!(!config.keep_open);
        assert_eq!(config.workspaces.len(), 1);
        assert_eq!(config.workspaces[0].name, "editor");
        assert_eq!(config.workspaces[0].spawn_at_startup.len(), 1);
        assert_eq!(
            config.workspaces[0].spawn_at_startup[0].command,
            vec!["foot", "-e", "nvim"]
        );
    }

    #[test]
    fn parse_keep_open() {
        let text = r#"
            project "writing" {
                keep-open
                workspace "notes" {
                    spawn-at-startup "obsidian"
                }
            }
        "#;
        let config = parse_project(text);
        assert!(config.keep_open);
    }

    #[test]
    fn parse_overview_border() {
        let text = r##"
            project "neovim" {
                overview-border {
                    color "#5aa9ff"
                }
                workspace "editor" {
                }
            }
        "##;
        let config = parse_project(text);
        let border = config.overview_border.expect("border should parse");
        assert_eq!(
            border.color.to_array_unpremul(),
            [90. / 255., 169. / 255., 1.0, 1.0]
        );
    }

    #[test]
    fn no_overview_border() {
        let text = r#"
            project "plain" {
                workspace "ws" {
                }
            }
        "#;
        let config = parse_project(text);
        assert!(config.overview_border.is_none());
    }

    #[test]
    fn parse_multiple_workspaces() {
        let text = r#"
            project "neovim" {
                workspace "editor" {
                    spawn-at-startup "foot" "-e" "nvim"
                }
                workspace "docs" {
                    spawn-at-startup "firefox" "--new-window" "http://localhost:3000"
                }
            }
        "#;
        let config = parse_project(text);
        assert_eq!(config.workspaces.len(), 2);
        assert_eq!(config.workspaces[0].name, "editor");
        assert_eq!(config.workspaces[1].name, "docs");
    }
}
