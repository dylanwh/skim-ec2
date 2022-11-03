use std::{borrow::Cow, collections::HashMap};

use aws_sdk_ec2 as ec2;
use colored_json::to_colored_json_auto;
use ec2::model::Instance;
use eyre::{eyre, Result};
use serde_json::{json, Value};
use skim::prelude::*;
use std::thread;

use clap::Parser;

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long, value_name = "NAME=VALUE")]
    filters: Vec<String>,

    #[arg(short, long, value_name = "NAME")]
    name_tag: Option<String>,

    #[arg(long)]
    name_host: bool,

    #[arg(short, long, value_name = "COMMAND")]
    command: Option<String>,
}

#[derive(Debug, Clone)]
enum NameRule {
    Tag(String),
    Host,
    InstanceID,
}

#[derive(Debug, Clone)]
struct InstanceItem {
    instance: Instance,
    name_rule: NameRule,
}

impl From<Instance> for InstanceItem {
    fn from(val: Instance) -> Self {
        InstanceItem {
            instance: val,
            name_rule: NameRule::InstanceID,
        }
    }
}

impl SkimItem for InstanceItem {
    fn text(&self) -> Cow<str> {
        match &self.name_rule {
            NameRule::Tag(tag) => {
                let tags = self.instance.tags.as_ref().unwrap();
                let name = tags
                    .iter()
                    .find(|t| t.key.as_ref().unwrap() == tag)
                    .unwrap()
                    .value
                    .as_ref()
                    .unwrap();
                Cow::from(name)
            }
            NameRule::Host => Cow::from(match self.instance.public_dns_name {
                Some(ref x) => x,
                None => match self.instance.private_dns_name {
                    Some(ref x) => x,
                    None => "",
                },
            }),
            NameRule::InstanceID => Cow::from(match self.instance.instance_id {
                Some(ref x) => x,
                None => "",
            }),
        }
    }

    fn display<'a>(&'a self, context: DisplayContext<'a>) -> AnsiString<'a> {
        AnsiString::from(context)
    }

    fn output(&self) -> Cow<str> {
        self.instance
            .instance_id
            .as_ref()
            .unwrap()
            .to_string()
            .into()
    }

    fn get_matching_ranges(&self) -> Option<&[(usize, usize)]> {
        None
    }

    fn preview(&self, _context: PreviewContext) -> ItemPreview {
        let instance_type = self.instance.instance_type.as_ref().unwrap();
        let instance_state = self
            .instance
            .state
            .as_ref()
            .unwrap()
            .name
            .as_ref()
            .unwrap()
            .as_str();
        let tags: HashMap<String, String> = self
            .instance
            .tags
            .as_ref()
            .unwrap()
            .iter()
            .map(|t| (t.key.as_ref().unwrap().to_string(), t.value.as_ref().unwrap().to_string()))
            .collect();

        let val: Value = json!({
            "instance_id":  self.instance.instance_id.as_ref().unwrap(),
            "instance_type": instance_type.as_str(),
            "state": instance_state,
            "public_dns_name": self.instance.public_dns_name.as_ref(),
            "private_dns_name":  self.instance.private_dns_name.as_ref(),
            "tags": tags
        });
        let s = to_colored_json_auto(&val).unwrap();
        ItemPreview::AnsiText(s)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let instances = get_instances(&args).await?;
    let options = SkimOptionsBuilder::default()
        .height(Some("50%"))
        .multi(false)
        .preview(Some("true"))
        .preview_window(Some("right:80%"))
        .build()
        .unwrap();

    let (s, r) = unbounded();

    thread::spawn(move || {
        for item in instances {
            let x: Arc<dyn SkimItem> = Arc::new(item.clone());
            s.send(x).unwrap();
        }
    });

    let instance_id = Skim::run_with(&options, Some(r))
        .map(|out| out.selected_items)
        .map(|items| {
            items
                .iter()
                .map(|item| item.output().to_string())
                .collect::<Vec<String>>()
        })
        .and_then(|ids| ids.first().map(String::to_string))
        .ok_or_else(|| eyre!("No instance selected"))?;

    if let Some(cmdline) = args.command {
        let id = shell_escape::escape(std::borrow::Cow::Borrowed(&instance_id));
        let cmdline = if cmdline.contains("{}") {
            cmdline.replace("{}", &id)
        } else {
            format!("{} {}", cmdline, id)
        };
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmdline)
            .status()?;
        if !status.success() {
            return Err(eyre!("Command failed"));
        }
    } else {
        println!("{}", instance_id);
    }
    Ok(())
}

async fn get_instances(args: &Args) -> Result<Vec<InstanceItem>> {
    let config = aws_config::load_from_env().await;
    let client = ec2::Client::new(&config);

    let mut instances_query = client.describe_instances();
    for f in args.filters.iter() {
        let filter = ec2::model::Filter::builder();
        instances_query = instances_query.filters(
            match f.split_once('=') {
                Some((name, value)) => filter.name(name).values(value),
                None => filter.name(f),
            }
            .build(),
        );
    }
    let output = instances_query.send().await?;
    let reservations = output
        .reservations()
        .ok_or_else(|| eyre!("no reservations"))?;
    let instances: Vec<InstanceItem> = reservations
        .iter()
        .flat_map(|r| r.instances().unwrap().iter().cloned())
        .map(|i| {
            let mut item: InstanceItem = i.into();
            if args.name_host {
                item.name_rule = NameRule::Host;
            }
            if let Some(ref tag) = args.name_tag {
                item.name_rule = NameRule::Tag(tag.to_string());
            }
            item
        })
        .collect();

    Ok(instances)
}