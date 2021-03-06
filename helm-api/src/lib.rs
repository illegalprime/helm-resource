#[macro_use] extern crate serde_derive;
extern crate rustache;
extern crate serde;
extern crate serde_json;
extern crate serde_yaml;
extern crate curl;
extern crate md5;
extern crate mktemp;
extern crate base64;
extern crate url;

mod error;

use std::collections::HashMap;
use self::error::HelmError;
use self::serde::Deserialize;
use self::serde_json::{
    Map,
    Value,
};
use self::curl::easy::Easy;
use self::md5::Context;
use self::mktemp::Temp;
use self::url::{
    Url,
    ParseError,
};
use self::rustache::{
    HashBuilder,
    Render,
};
use std::io::{
    Write,
    self,
};
use std::fs::File;
use std::process::Command;


const KUBE_CONFIG: &'static str = include_str!("../templates/kube-config.mo");
const SH_PATH: &'static str = "/bin/sh";


#[derive(Debug, Serialize, Deserialize)]
pub struct Chart {
    pub release: String,
    pub name: String,
    pub version: Option<String>,
    pub overrides: Option<HashMap<String, Value>>,
}

pub type Charts = Vec<Chart>;

pub struct Helm {
    namespace: String,
    server: String,
    username: String,
    password: String,
    kube_config: Temp,
    kube_ca_cert: Option<Temp>,
}

pub struct Config {
    pub url: String,
    pub username: String,
    pub password: String,
    pub namespace: String,
    pub skip_tls_verify: Option<bool>,
    pub ca_data: Option<String>,
}

impl Helm {
    pub fn configure(config: Config) -> Result<Self, HelmError> {
        // check invariants
        if config.ca_data.is_none() && !config.skip_tls_verify.unwrap_or(false) {
            return Err(HelmError::NoCaData);
        }

        // we'll store this config file for helm to use
        let kube_config_path = try!(Temp::new_file());
        let mut kube_config_file = try!(File::create(&kube_config_path));
        let base_64_ca_data = config.ca_data
            .as_ref()
            .map(|c| base64::encode(c.trim().as_bytes()));

        // generate k8s config file so helm can connect to our server
        try!(HashBuilder::new()
            .insert("skip_tls_verify", config.skip_tls_verify.unwrap_or(false))
            .insert("url", &config.url as &str)
            .insert("namespace", &config.namespace as &str)
            .insert("username", &config.username as &str)
            .insert("password", &config.password as &str)
            .insert("ca_data", base_64_ca_data.as_ref().map(|s| s as &str).unwrap_or(""))
            .render(KUBE_CONFIG, &mut kube_config_file));

        // make sure we wrote the file
        try!(kube_config_file.flush());

        // create a file to store the ca data for the kubes api
        let ca_cert_path = if let Some(ref ca_data) = config.ca_data {
            let ca_cert_path = try!(Temp::new_file());
            let mut ca_cert_file = try!(File::create(&ca_cert_path));
            try!(ca_cert_file.write_all(ca_data.as_bytes()));
            try!(ca_cert_file.flush());
            Some(ca_cert_path)
        } else {
            None
        };

        let helm = Helm {
            namespace: config.namespace,
            server: config.url,
            username: config.username,
            password: config.password,
            kube_config: kube_config_path,
            kube_ca_cert: ca_cert_path,
        };

        // init helm
        try!(helm.run("helm init --client-only 1>&2"));

        // update helm repos
        try!(helm.run("helm repo update"));

        Ok(helm)
    }

    fn run(&self, cmd: &str) -> Result<String, HelmError> {
        // log the command we're running
        try!(io::stderr().write(format!("Running `{}`.\n", cmd).as_bytes()));

        let output = try!(Command::new(SH_PATH)
            .env("KUBECONFIG", &self.kube_config.to_path_buf().to_string_lossy().into_owned())
            .arg("-c")
            .arg(cmd)
            .output());

        // log things to stderr since stdout is reserved
        try!(io::stderr().write(&output.stdout));
        try!(io::stderr().write(&output.stderr));
        try!(io::stderr().flush());

        if !output.status.success() {
            return Err(HelmError::CmdFailed(cmd.to_string()));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn kube_api<D>(&self, url: &str) -> Result<D, HelmError>
    where D: Deserialize,
    {
        let mut handle = Easy::new();

        try!(handle.url(&url));
        try!(handle.username(&self.username));
        try!(handle.password(&self.password));

        if let Some(ref ca_cert_path) = self.kube_ca_cert {
            try!(handle.cainfo(ca_cert_path));
        } else {
            try!(handle.ssl_verify_peer(false));
        }

        let mut buf = Vec::new();
        {
            let mut transfer = handle.transfer();
            try!(transfer.write_function(|data| {
                buf.extend_from_slice(data);
                Ok(data.len())
            }));
            try!(transfer.perform());
        }

        match serde_json::from_str::<D>(String::from_utf8_lossy(&buf).trim()) {
            Ok(v) => Ok(v),
            Err(_) => unimplemented!(),
        }
    }

    pub fn list(&self) -> Result<Vec<Chart>, HelmError> {
        // get the api endpoint
        let mut deployments_api = try!(Url::parse(&self.server));
        try!(deployments_api.path_segments_mut().map(|mut segments| {
            segments
                .extend("apis/extensions/v1beta1/namespaces".split('/'))
                .push(&self.namespace)
                .push("deployments");
        })
        .map_err(|_| HelmError::UrlParse(
            ParseError::RelativeUrlWithCannotBeABaseBase)));

        let deployments: Map<String, Value> = try!(self.kube_api(&deployments_api.into_string()));

        Ok(deployments
            .get("items")
            .and_then(Value::as_array)
            .map_or(Vec::new(), |items| {
                items.iter()
                    .map(Value::as_object).filter_map(|i| i)
                    .map(|o| o.get("metadata")).filter_map(|i| i)
                    .map(Value::as_object).filter_map(|i| i)
                    .filter(|metadata| {
                        metadata
                            .get("namespace")
                            .and_then(Value::as_str)
                            .map(|n| n == self.namespace)
                            .unwrap_or(false)
                    })
                    .map(|o| o.get("labels")).filter_map(|i| i)
                    .map(Value::as_object).filter_map(|i| i)
                    .filter(|labels| {
                        labels
                            .get("heritage")
                            .and_then(Value::as_str)
                            .map(|n| n == "Tiller")
                            .unwrap_or(false)
                    })
                    .map(|labels| {
                        labels.get("release")
                            .and_then(Value::as_str)
                            .and_then(|release| {
                            labels.get("chart")
                                .and_then(Value::as_str)
                                .map(|c| c.rsplitn(2, '-'))
                                .and_then(|mut split| {
                                    split.next().and_then(|version| {
                                        split.last().map(|chart_name| {
                                            Chart {
                                                release: release.to_string(),
                                                name: chart_name.to_string(),
                                                version: Some(version.to_string()),
                                                overrides: None,
                                            }
                                        })
                                    })
                                })
                        })
                    })
                    .filter_map(|i| i)
                    .collect()
            }))
    }

    pub fn digest(&self) -> Result<String, HelmError> {
        let mut hash = Context::new();
        for chart in try!(self.list()) {
            hash.consume(chart.release);
            hash.consume(chart.name);
            if let Some(version) = chart.version {
                hash.consume(version);
            }
        }
        Ok(format!("{:x}", hash.compute()))
    }

    pub fn upgrade(&self, chart: &Chart) -> Result<(), HelmError> {
        let mut cmd = vec![];

        // start of the command
        cmd.push(format!("helm upgrade -i --namespace {}", self.namespace));

        if let Some(ref version) = chart.version {
            cmd.push(format!("--version {}", version));
        }

        let overrides_file = if let Some(ref overrides) = chart.overrides {
            let override_path = try!(Temp::new_file());

            // set values file flag
            cmd.push(format!("--values {}",
                override_path.to_path_buf().to_string_lossy().into_owned()));

            // write the overrides to the file
            let mut overrides_file = try!(File::create(&override_path));
            try!(serde_yaml::to_writer(&mut overrides_file, overrides));
            try!(overrides_file.flush());

            // log values used
            try!(io::stderr().write_fmt(format_args!("Using values:\n{}\n",
                try!(serde_yaml::to_string(overrides)))));

            Some(override_path)
        } else {
            None
        };

        // end of the command
        cmd.push(format!("{} stable/{}", chart.release, chart.name));

        try!(self.run(&cmd.join(" ")).map(|_| { () }));

        // cleanup resources
        if let Some(mut file) = overrides_file {
            file.release();
        }
        Ok(())
    }

    pub fn delete(&self, release: &str) -> Result<(), HelmError> {
        let cmd = format!("helm delete {}", release);
        self.run(&cmd).map(|_| { () })
    }
}
