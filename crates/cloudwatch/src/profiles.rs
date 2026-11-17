use aws_config::profile::Profile;
use aws_types::os_shim_internal::{Env, Fs};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileKind {
    /// `aws login`: short-lived console credentials refreshed from
    /// `~/.aws/login/cache` until the session itself expires.
    Login,
    Sso,
    AccessKeys,
    CredentialProcess,
    AssumeRole,
    /// Settings only (region, output) with credentials coming from elsewhere,
    /// such as environment variables.
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsProfile {
    pub name: String,
    pub kind: ProfileKind,
    pub region: Option<String>,
}

/// Reads `~/.aws/config` and `~/.aws/credentials` the same way the SDK does,
/// honoring `AWS_CONFIG_FILE` and `AWS_SHARED_CREDENTIALS_FILE`.
pub async fn load_profiles() -> anyhow::Result<Vec<AwsProfile>> {
    load_profiles_from(&Fs::real(), &Env::real()).await
}

async fn load_profiles_from(fs: &Fs, env: &Env) -> anyhow::Result<Vec<AwsProfile>> {
    let profiles = aws_config::profile::load(fs, env, &Default::default(), None).await?;
    let mut result: Vec<AwsProfile> = profiles
        .profiles()
        .filter_map(|name| {
            let profile = profiles.get_profile(name)?;
            Some(AwsProfile {
                name: name.to_string(),
                kind: kind_of(profile),
                region: profile.get("region").map(str::to_string),
            })
        })
        .collect();
    result.sort_by(|a, b| (a.name != "default", &a.name).cmp(&(b.name != "default", &b.name)));
    Ok(result)
}

fn kind_of(profile: &Profile) -> ProfileKind {
    if profile.get("login_session").is_some() {
        ProfileKind::Login
    } else if profile.get("sso_session").is_some() || profile.get("sso_start_url").is_some() {
        ProfileKind::Sso
    } else if profile.get("role_arn").is_some() {
        ProfileKind::AssumeRole
    } else if profile.get("credential_process").is_some() {
        ProfileKind::CredentialProcess
    } else if profile.get("aws_access_key_id").is_some() {
        ProfileKind::AccessKeys
    } else {
        ProfileKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_kind_and_region_of_each_profile() {
        let fs = Fs::from_slice(&[
            (
                "/home/aws/.aws/config",
                "[default]\n\
                 login_session = arn:aws:iam::123456789012:user/someone\n\
                 region = us-east-1\n\
                 [profile staging]\n\
                 sso_session = company\n\
                 sso_account_id = 123456789012\n\
                 sso_role_name = ReadOnly\n\
                 region = sa-east-1\n\
                 [profile ci]\n\
                 region = us-west-2\n\
                 [sso-session company]\n\
                 sso_start_url = https://company.awsapps.com/start\n\
                 sso_region = us-east-1\n",
            ),
            (
                "/home/aws/.aws/credentials",
                "[ci]\naws_access_key_id = AKIA000\naws_secret_access_key = secret\n",
            ),
        ]);
        let env = Env::from_slice(&[("HOME", "/home/aws")]);

        let profiles = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(load_profiles_from(&fs, &env))
            .expect("profiles");

        assert_eq!(
            profiles,
            vec![
                AwsProfile {
                    name: "default".into(),
                    kind: ProfileKind::Login,
                    region: Some("us-east-1".into()),
                },
                AwsProfile {
                    name: "ci".into(),
                    kind: ProfileKind::AccessKeys,
                    region: Some("us-west-2".into()),
                },
                AwsProfile {
                    name: "staging".into(),
                    kind: ProfileKind::Sso,
                    region: Some("sa-east-1".into()),
                },
            ]
        );
    }
}
