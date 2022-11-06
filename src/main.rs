#![warn(
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unused_async
)]
// allow expect
#![allow(clippy::expect_used)]

use aws_sdk_ec2 as ec2;
use clap::Parser;
use colored_json::to_colored_json_auto;
use ec2::{model::Instance, Client};
use eyre::{eyre, Result};
use serde_json::{json, Value};
use skim::prelude::*;
use std::{borrow::Cow, collections::HashMap};

#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long, value_name = "NAME=VALUE")]
    filter: Vec<String>,

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

#[derive(Debug, Clone)]
struct ErrorItem {
    message: String,
}

impl From<Instance> for InstanceItem {
    fn from(val: Instance) -> Self {
        Self {
            instance: val,
            name_rule: NameRule::InstanceID,
        }
    }
}

impl SkimItem for ErrorItem {
    fn text(&self) -> Cow<str> {
        Cow::from("error")
    }

    fn preview(&self, _context: PreviewContext) -> ItemPreview {
        ItemPreview::Text(self.message.clone())
    }
}

impl SkimItem for InstanceItem {
    fn text(&self) -> Cow<str> {
        match &self.name_rule {
            NameRule::Tag(tag) => {
                let tags = self.instance.tags.as_ref().expect("instance has no tags");
                let name = tags
                    .iter()
                    .find(|t| t.key == Some(tag.to_string()))
                    .expect("tag for name not found")
                    .value
                    .as_ref()
                    .expect("tag for name has no value");
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

    fn preview(&self, _context: PreviewContext) -> ItemPreview {
        let instance_type = self
            .instance
            .instance_type
            .as_ref()
            .expect("instance has no type");
        let instance_state = self
            .instance
            .state
            .as_ref()
            .expect("instance has no state")
            .name
            .as_ref()
            .expect("instance state name")
            .as_str();
        let tags: HashMap<String, String> = self
            .instance
            .tags
            .as_ref()
            .expect("instance tags")
            .iter()
            .map(|t| {
                return (
                    t.key.as_ref().expect("tag key").to_string(),
                    t.value.as_ref().expect("tag value").to_string(),
                );
            })
            .collect();

        let uptime = match self.instance.launch_time {
            Some(ref x) => {
                let secs = x.secs();
                let now = chrono::Utc::now().timestamp();
                let uptime = secs - now;
                let uptime = chrono::Duration::seconds(uptime);
                let uptime = chrono_humanize::HumanTime::from(uptime);
                format!("{}", uptime)
            }
            None => String::new(),
        };

        let val: Value = json!({
            "instance_id":  self.instance.instance_id.as_ref().expect("instance id"),
            "instance_type": instance_type.as_str(),
            "state": instance_state,
            "uptime": uptime,
            "public_dns_name": self.instance.public_dns_name.as_ref(),
            "private_dns_name":  self.instance.private_dns_name.as_ref(),
            "tags": tags
        });
        let s = to_colored_json_auto(&val).unwrap_or_else(|_| String::new());
        ItemPreview::AnsiText(s)
    }

    fn output(&self) -> Cow<str> {
        return self
            .instance
            .instance_id
            .as_ref()
            .expect("instance has no id")
            .to_string()
            .into();
    }

    fn get_matching_ranges(&self) -> Option<&[(usize, usize)]> {
        None
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let config = aws_config::load_from_env().await;
    // verify credentials
    let _ = config.credentials_provider();

    let client = ec2::Client::new(&config);

    let options = SkimOptionsBuilder::default()
        .height(Some("100%"))
        .multi(false)
        .preview(Some("true"))
        .preview_window(Some("right:70%"))
        .build()
        .expect("failed to build skim options");

    let args = Arc::new(args);
    let r = get_instances_background(Arc::new(client), args.clone()).await;

    let output = Skim::run_with(&options, Some(r)).ok_or_else(|| eyre!("No output from skim"))?;
    let instance_id: String = if output.is_abort {
        Err(eyre!("skim aborted"))
    } else {
        Ok(output
            .selected_items
            .first()
            .expect("selected item")
            .output()
            .to_string())
    }?;

    if let Some(cmdline) = args.command.as_ref() {
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

async fn get_instances_background(
    client: Arc<Client>,
    args: Arc<Args>,
) -> SkimItemReceiver {
    let (s, r) = unbounded();
    tokio::spawn(async move {
        match get_instances(&client, &args).await {
            Ok(instances) => {
                for item in instances {
                    let x: Arc<dyn SkimItem> = Arc::new(item.clone());
                    s.send(x).expect("send error");
                }
            }
            Err(msg) => {
                let x: Arc<dyn SkimItem> = Arc::new(ErrorItem {
                    message: format!("{}", msg),
                });
                s.send(x).expect("send error");
            }
        }
    });

    r
}

async fn get_instances(client: &Client, args: &Args) -> Result<Vec<InstanceItem>> {
    let mut instances_query = client.describe_instances();
    for f in &args.filter {
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
        .flat_map(|r| r.instances().expect("instances").iter().cloned())
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
