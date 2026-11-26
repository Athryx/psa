//! Pi-compatible skill catalogue text sent to the model.

use super::escape_xml;

/// Metadata only: the skill body is loaded separately when needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDescription {
    pub name: String,
    pub description: String,
    pub location: String,
}

pub fn format_skills(skills: &[SkillDescription]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }

    let mut lines = vec![
        "<skills>".to_owned(),
        "The following skills provide specialized instructions for specific tasks.".to_owned(),
        "Load a skill's file when the task matches its description.".to_owned(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_owned(),
        String::new(),
        "<available_skills>".to_owned(),
    ];
    for skill in skills {
        lines.push("  <skill>".into());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.location)
        ));
        lines.push("  </skill>".into());
    }
    lines.push("</available_skills>".into());
    lines.push("</skills>".into());
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_catalogue_has_no_prompt() {
        assert_eq!(format_skills(&[]), None);
    }

    #[test]
    fn pi_catalogue_preserves_order_and_escapes_every_field() {
        let first = SkillDescription {
            name: "a&<>\"'".into(),
            description: "first\n&<>\"'".into(),
            location: "/a&<>\"'/SKILL.md".into(),
        };
        let second = SkillDescription {
            name: "second".into(),
            description: "second description".into(),
            location: "/second/SKILL.md".into(),
        };
        let prompt = format_skills(&[first, second]).unwrap();
        assert_eq!(
            prompt,
            concat!(
                "<skills>\n",
                "The following skills provide specialized instructions for specific tasks.\n",
                "Load a skill's file when the task matches its description.\n",
                "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\n",
                "<available_skills>\n",
                "  <skill>\n",
                "    <name>a&amp;&lt;&gt;&quot;&apos;</name>\n",
                "    <description>first\n&amp;&lt;&gt;&quot;&apos;</description>\n",
                "    <location>/a&amp;&lt;&gt;&quot;&apos;/SKILL.md</location>\n",
                "  </skill>\n",
                "  <skill>\n",
                "    <name>second</name>\n",
                "    <description>second description</description>\n",
                "    <location>/second/SKILL.md</location>\n",
                "  </skill>\n",
                "</available_skills>\n",
                "</skills>"
            )
        );
    }
}
