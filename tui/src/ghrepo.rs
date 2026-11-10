//! Read/write access to one repository on GitHub, GitLab, Bitbucket (Cloud or Data Center) or
//! Forgejo: parsing the remote, which provider a host is (asked once, remembered), the token
//! page and steps, the check request, names, and the provider profile that lets `git push`
//! reach that repo only.
//!
//! Only GitHub's git-over-HTTPS path has run against the real service through OpenShell's
//! proxy. For GitLab, Bitbucket and Forgejo the profiles, the credential helper usernames and
//! the check requests follow the providers' documentation and still need a live test.

/// Days a freshly created token lives; also what the template URL asks GitHub for.
pub const TOKEN_DAYS: u32 = 30;

/// Warn about, and offer to renew, a token this many days before it expires.
pub const WARN_DAYS: u64 = 7;

/// The host GitHub repos live on; osh supports GitHub there only.
pub const GITHUB_HOST: &str = "github.com";

/// Bitbucket Cloud's git host. Any other host the user calls Bitbucket is a Data Center one.
pub const BITBUCKET_CLOUD_HOST: &str = "bitbucket.org";

/// Bitbucket Cloud's API lives on its own host, unlike the other providers'.
pub const BITBUCKET_API_HOST: &str = "api.bitbucket.org";

/// Bitbucket Data Center's default SSH port.
pub const BITBUCKET_DC_SSH_PORT: u16 = 7999;

/// The last item of "Which git provider hosts HOST?".
pub const NO_PROVIDER_ITEM: &str = "None / other (no repo access)";

/// Who hosts a repository. Bitbucket is split by host because Cloud and Data Center differ in
/// URLs, tokens and API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Forge {
    GitHub,
    GitLab,
    BitbucketCloud,
    BitbucketDc,
    Forgejo,
}

impl Forge {
    /// The word kept in the hosts file; both Bitbucket kinds are `bitbucket`, told apart by host.
    pub fn key(self) -> &'static str {
        match self {
            Forge::GitHub => "github",
            Forge::GitLab => "gitlab",
            Forge::BitbucketCloud | Forge::BitbucketDc => "bitbucket",
            Forge::Forgejo => "forgejo",
        }
    }

    /// The provider `key` names for `host`; `None` for an unknown word (or `none`).
    pub fn from_key(key: &str, host: &str) -> Option<Forge> {
        Some(match key {
            "github" => Forge::GitHub,
            "gitlab" => Forge::GitLab,
            "bitbucket" if host == BITBUCKET_CLOUD_HOST => Forge::BitbucketCloud,
            "bitbucket" => Forge::BitbucketDc,
            "forgejo" => Forge::Forgejo,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Forge::GitHub => "GitHub",
            Forge::GitLab => "GitLab",
            Forge::BitbucketCloud => "Bitbucket",
            Forge::BitbucketDc => "Bitbucket Data Center",
            Forge::Forgejo => "Forgejo",
        }
    }

    /// The start of every repo provider name for this provider.
    fn prefix(self) -> &'static str {
        match self {
            Forge::GitHub => "github",
            Forge::GitLab => "gitlab",
            Forge::BitbucketCloud | Forge::BitbucketDc => "bitbucket",
            Forge::Forgejo => "forgejo",
        }
    }

    /// The start of the generated profile id.
    fn id_prefix(self) -> &'static str {
        match self {
            Forge::GitHub => "gh",
            Forge::GitLab => "gl",
            Forge::BitbucketCloud | Forge::BitbucketDc => "bb",
            Forge::Forgejo => "fj",
        }
    }

    /// The credential variable of a repo provider: what the sandbox sees as its placeholder.
    pub fn token_env(self) -> &'static str {
        match self {
            Forge::GitHub => "GITHUB_TOKEN",
            Forge::GitLab => "GITLAB_TOKEN",
            Forge::BitbucketCloud | Forge::BitbucketDc => "BITBUCKET_TOKEN",
            Forge::Forgejo => "FORGEJO_TOKEN",
        }
    }

    /// The Basic-auth username git sends with the token as password. GitLab takes any
    /// non-empty name (`oauth2` by convention); Bitbucket repository tokens need
    /// `x-token-auth`; Forgejo ignores the name when the password is a token (unconfirmed by
    /// Forgejo's docs, so it needs a live test).
    pub fn git_user(self) -> &'static str {
        match self {
            Forge::GitHub => "x-access-token",
            Forge::GitLab | Forge::Forgejo => "oauth2",
            Forge::BitbucketCloud | Forge::BitbucketDc => "x-token-auth",
        }
    }

    /// The shipped read-only provider profile, where osh has one.
    pub fn read_profile(self) -> Option<&'static str> {
        match self {
            Forge::GitHub => Some("github"),
            Forge::Forgejo => Some("forgejo"),
            _ => None,
        }
    }

    /// Whether osh tracks the token's expiry. Forgejo tokens never expire. Bitbucket may allow
    /// "No expiry", but the steps ask for 30 days, so osh warns after 30 days either way.
    pub fn expires(self) -> bool {
        self != Forge::Forgejo
    }
}

/// The items of "Which git provider hosts HOST?": the likely one first, "none" last.
pub fn provider_choices(host: &str) -> Vec<String> {
    let likely = match host {
        "gitlab.com" => "GitLab",
        BITBUCKET_CLOUD_HOST => "Bitbucket",
        "codeberg.org" => "Forgejo",
        _ => "GitHub",
    };
    std::iter::once(likely)
        .chain(
            ["GitHub", "GitLab", "Bitbucket", "Forgejo"]
                .into_iter()
                .filter(|p| *p != likely),
        )
        .chain(std::iter::once(NO_PROVIDER_ITEM))
        .map(String::from)
        .collect()
}

/// The provider an item of `provider_choices` names; `None` for "None / other".
pub fn chosen_forge(item: &str, host: &str) -> Option<Forge> {
    Forge::from_key(&item.to_ascii_lowercase(), host)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GhRepo {
    pub forge: Forge,
    pub host: String,
    /// The owner, GitLab group path (`a/b/c`), Bitbucket workspace or Data Center project key.
    pub owner: String,
    pub repo: String,
}

impl GhRepo {
    pub fn full(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    pub fn is_github(&self) -> bool {
        self.forge == Forge::GitHub
    }

    /// Where the repo is on screen: `OWNER/REPO` for GitHub, `HOST/OWNER/REPO` otherwise, so
    /// another host's repo is never mistaken for a GitHub one.
    pub fn shown(&self) -> String {
        if self.is_github() {
            self.full()
        } else {
            format!("{}/{}", self.host, self.full())
        }
    }

    /// The form kept in the tracked-tokens file: `OWNER/REPO` for GitHub (as before other
    /// providers existed), `KEY:HOST/OWNER/REPO` otherwise. `from_spec` reads it back.
    pub fn spec(&self) -> String {
        if self.is_github() {
            self.full()
        } else {
            format!("{}:{}", self.forge.key(), self.shown())
        }
    }

    pub fn from_spec(s: &str) -> Option<GhRepo> {
        let Some((key, rest)) = s.split_once(':') else {
            return parse_remote(&format!("https://{GITHUB_HOST}/{s}"));
        };
        let (host, path) = rest.split_once('/')?;
        let segs: Vec<&str> = path.split('/').collect();
        let (repo, owner) = segs.split_last()?;
        make(Forge::from_key(key, host)?, host, owner, repo)
    }

    /// Paths of `POST git-receive-pack` for this repo, with and without `.git`.
    fn push_paths(&self) -> [String; 2] {
        let base = match self.forge {
            Forge::BitbucketDc => format!("/scm/{}", self.full()),
            _ => format!("/{}", self.full()),
        };
        [
            format!("{base}.git/git-receive-pack"),
            format!("{base}/git-receive-pack"),
        ]
    }
}

/// A bare hostname: dot-separated labels of letters, digits and dashes; no scheme, port or path.
pub fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// A path segment safe to put in a URL, a profile path and YAML: letters, digits, `-_.`, and
/// no `..`. A leading `~` is allowed only for a Data Center personal project (`~USER`).
fn valid_segment(s: &str, tilde: bool) -> bool {
    let body = if tilde {
        s.strip_prefix('~').unwrap_or(s)
    } else {
        s
    };
    !body.is_empty()
        && body != "."
        && !body.contains("..")
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A repo of `forge` on `host`, after checking every part. GitLab owners may be nested groups;
/// the others have exactly one owner segment.
fn make(forge: Forge, host: &str, owner: &[&str], repo: &str) -> Option<GhRepo> {
    let host_ok = match forge {
        Forge::GitHub => host == GITHUB_HOST,
        Forge::BitbucketCloud => host == BITBUCKET_CLOUD_HOST,
        Forge::BitbucketDc => host != BITBUCKET_CLOUD_HOST,
        _ => true,
    };
    let owners_ok = match forge {
        Forge::GitLab => !owner.is_empty(),
        _ => owner.len() == 1,
    };
    let dc = forge == Forge::BitbucketDc;
    let ok = host_ok
        && owners_ok
        && valid_host(host)
        && owner
            .iter()
            .enumerate()
            .all(|(i, s)| valid_segment(s, dc && i == 0))
        && valid_segment(repo, false);
    ok.then(|| GhRepo {
        forge,
        host: host.into(),
        owner: owner.join("/"),
        repo: repo.into(),
    })
}

/// A git remote before its provider is known: host, path segments (without `.git`), and
/// whether it is ssh (and on which port), which decides the Data Center path shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub host: String,
    pub ssh: bool,
    pub ssh_port: Option<u16>,
    pub path: Vec<String>,
}

impl Remote {
    /// The repo this remote names on `forge`, or `None` when the path does not fit it (a
    /// Data Center https remote must be `/scm/PROJECT/REPO`; a context path is not supported).
    pub fn repo(&self, forge: Forge) -> Option<GhRepo> {
        let mut segs: Vec<&str> = self.path.iter().map(String::as_str).collect();
        if forge == Forge::BitbucketDc && !self.ssh {
            if segs.first() != Some(&"scm") {
                return None;
            }
            segs.remove(0);
        }
        let (repo, owner) = segs.split_last()?;
        make(forge, &self.host, owner, repo)
    }
}

/// A remote URL on any host: scp-style (`git@host:o/r.git`), `ssh://[user@]host[:port]/o/r`
/// or `https://[user@]host/o/r`. An https remote on a custom port is refused, because the
/// generated profile reaches port 443 only; an ssh port is kept so the shell can rewrite that
/// remote to https.
pub fn parse_url(url: &str) -> Option<Remote> {
    let url = url.trim();
    let (host, port, ssh, path) = if let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    {
        let (auth, path) = rest.split_once('/')?;
        (
            auth.rsplit_once('@').map_or(auth, |(_, h)| h),
            None,
            false,
            path,
        )
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let (auth, path) = rest.split_once('/')?;
        let hostport = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h, Some(p.parse::<u16>().ok()?), true, path),
            None => (hostport, None, true, path),
        }
    } else if url.contains("://") {
        return None;
    } else {
        let (auth, path) = url.split_once(':')?;
        if auth.contains('/') {
            return None;
        }
        (
            auth.rsplit_once('@').map_or(auth, |(_, h)| h),
            None,
            true,
            path,
        )
    };
    let host = host.to_ascii_lowercase();
    if !valid_host(&host) {
        return None;
    }
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path: Vec<String> = path.split('/').map(String::from).collect();
    path.iter().all(|s| valid_segment(s, true)).then(|| Remote {
        host,
        ssh,
        ssh_port: port.filter(|p| *p != 22),
        path,
    })
}

/// The GitHub repository a remote URL points at: `git@github.com:o/r.git`,
/// `ssh://git@github.com/o/r.git` or `https://github.com/o/r`.
pub fn parse_remote(url: &str) -> Option<GhRepo> {
    parse_url(url)?.repo(Forge::GitHub)
}

/// Lowercase letters, digits and single dashes: safe in provider and profile names.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Shared by every provider that grants this repo: the profile holds the push rule.
pub fn profile_id(r: &GhRepo) -> String {
    if r.is_github() {
        format!("gh-{}-{}", slug(&r.owner), slug(&r.repo))
    } else {
        format!(
            "{}-{}-{}-{}",
            r.forge.id_prefix(),
            slug(&r.host),
            slug(&r.owner),
            slug(&r.repo)
        )
    }
}

/// The credential variable of a repo provider: what the sandbox sees as its placeholder.
pub fn token_env(r: &GhRepo) -> &'static str {
    r.forge.token_env()
}

/// One token for one sandbox and repo.
pub fn provider_name(sandbox: &str, r: &GhRepo) -> String {
    format!("{}-{}-{}", r.forge.prefix(), slug(sandbox), slug(&r.repo))
}

/// The start of `provider_name` for a sandbox, to find its own providers again.
pub fn sandbox_prefix(forge: Forge, sandbox: &str) -> String {
    format!("{}-{}-", forge.prefix(), slug(sandbox))
}

/// One token for one repo, attached to any number of sandboxes.
pub fn shared_provider_name(r: &GhRepo) -> String {
    format!("{}-{}", r.forge.prefix(), slug(&r.repo))
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The page osh opens to create the token. Only GitHub's is a documented, prefilled template;
/// GitLab's project form may take `name` and `scopes` (evidenced by a merge request only, so
/// the steps stand on their own); Bitbucket and Forgejo pages are not prefilled.
pub fn token_url(r: &GhRepo, label: &str) -> String {
    let name = token_name(label);
    match r.forge {
        Forge::GitHub => format!(
            "https://github.com/settings/personal-access-tokens/new?name={}&description={}&target_name={}&expires_in={TOKEN_DAYS}&contents=write&pull_requests=write&workflows=write",
            encode(&name),
            encode(&format!("osh sandbox access to {}", r.full())),
            encode(&r.owner),
        ),
        Forge::GitLab => format!(
            "https://{}/{}/-/settings/access_tokens?name={}&scopes=read_api,write_repository",
            r.host,
            r.full(),
            encode(&name)
        ),
        // The direct path is not on Atlassian's pages; the steps also give the menu path.
        Forge::BitbucketCloud => format!("https://{}/{}/admin/access-tokens", r.host, r.full()),
        Forge::BitbucketDc => repo_settings_dc(r),
        Forge::Forgejo => forgejo_token_url(&r.host),
    }
}

/// A Data Center repository's settings page (assumes no context path).
fn repo_settings_dc(r: &GhRepo) -> String {
    format!(
        "https://{}/projects/{}/repos/{}/settings",
        r.host, r.owner, r.repo
    )
}

/// The name the token pages are told to give a token labelled `label`.
pub fn token_name(label: &str) -> String {
    let name: String = format!("osh-{}", slug(label)).chars().take(40).collect();
    name.trim_end_matches('-').to_string()
}

/// Where GitHub lists fine-grained tokens; it has no API to delete one.
pub const TOKENS_PAGE: &str = "https://github.com/settings/personal-access-tokens";

/// Forgejo's page for a new access token; it cannot be prefilled.
pub fn forgejo_token_url(host: &str) -> String {
    format!("https://{host}/user/settings/applications/tokens/new")
}

/// Where a Forgejo user lists, and can delete, access tokens.
pub fn forgejo_tokens_page(host: &str) -> String {
    format!("https://{host}/user/settings/applications")
}

/// Where the user lists, and can revoke, the token made for `r`.
pub fn tokens_page(r: &GhRepo) -> String {
    match r.forge {
        Forge::GitHub => TOKENS_PAGE.into(),
        Forge::GitLab => format!("https://{}/{}/-/settings/access_tokens", r.host, r.full()),
        Forge::BitbucketCloud => format!(
            "https://{}/{}/admin/access-tokens (Repository settings > Security > Access tokens)",
            r.host,
            r.full()
        ),
        Forge::BitbucketDc => format!("{} (HTTP access tokens)", repo_settings_dc(r)),
        Forge::Forgejo => forgejo_tokens_page(&r.host),
    }
}

/// The steps for a token labelled `label`, by the repo's provider.
pub fn steps_for(r: &GhRepo, url: &str, label: &str) -> String {
    let name = token_name(label);
    let full = r.full();
    let host = &r.host;
    match r.forge {
        Forge::GitHub => steps(r, url),
        Forge::GitLab => format!(
            "Create a project access token for {full} on {host}:\n\
             1. Open {url} (the project's Settings > Access tokens > Add new token).\n\
             2. Token name: {name}. Role: Developer (Maintainer if the branch you push to is protected for Maintainers).\n\
             3. Scopes: read_api and write_repository, nothing else (read_api lets osh check the token, write_repository lets git pull and push).\n\
             4. Expiration date: {TOKEN_DAYS} days from today. Create the token and copy it.\n\
             On gitlab.com, project access tokens need Premium or Ultimate. On the Free tier create a personal access token at https://{host}/-/user_settings/personal_access_tokens with the same scopes and expiry instead: it reaches every project your account can, not only {full}."
        ),
        Forge::BitbucketCloud => format!(
            "Create a repository access token for {full} on {host}:\n\
             1. Open {url} (or the repository's Repository settings > Security > Access tokens), then Create access token.\n\
             2. Name: {name}.\n\
             3. Permissions: Repositories Read and Write, nothing else.\n\
             4. Expiry: {TOKEN_DAYS} days if your workspace lets you choose (osh warns after {TOKEN_DAYS} days either way). Create the token and copy it.\n\
             App passwords no longer work on Bitbucket Cloud."
        ),
        Forge::BitbucketDc => format!(
            "Create a repository HTTP access token for {full} on {host}:\n\
             1. Open {url}, then Repository settings > HTTP access tokens > Create token.\n\
             2. Token name: {name}.\n\
             3. Permissions: Repository write (it includes read), nothing broader.\n\
             4. Expiry: {TOKEN_DAYS} days if offered (osh warns after {TOKEN_DAYS} days either way). Create the token and copy it."
        ),
        Forge::Forgejo => format!(
            "Create an access token for {full} on {host}:\n\
             1. Open {url}\n\
             2. Token name: {name}.\n\
             3. Repository and organization access: Specific repositories, then {full}.\n\
             4. Select the scope write:repository and nothing broader. Generate the token and copy it.\n\
             Forgejo tokens do not expire: delete it at {page} when no sandbox needs it.",
            page = forgejo_tokens_page(host),
        ),
    }
}

/// What to tell the user before they paste a GitHub token.
pub fn steps(r: &GhRepo, url: &str) -> String {
    format!(
        "Create a fine-grained token for {full}:\n\
         1. Open {url}\n\
         2. Resource owner: {owner}. Repository access: Only select repositories, then {full}.\n\
         3. Keep Contents, Pull requests and Workflows at Read and write (GitHub rejects a push that touches .github/workflows without Workflows).\n\
         4. Expiration: {TOKEN_DAYS} days. Generate the token and copy it.\n\
         An organization may need to approve the token before it works.",
        full = r.full(),
        owner = r.owner,
    )
}

/// How osh checks a pasted token can see `r`: the header line the token follows (written to a
/// private header file, never argv), the Accept header, and the URL of a read-only GET.
pub fn check_request(r: &GhRepo) -> (&'static str, &'static str, String) {
    const BEARER: &str = "Authorization: Bearer ";
    const JSON: &str = "Accept: application/json";
    match r.forge {
        Forge::GitHub => (
            BEARER,
            "Accept: application/vnd.github+json",
            format!("https://api.github.com/repos/{}", r.full()),
        ),
        Forge::GitLab => (
            "PRIVATE-TOKEN: ",
            JSON,
            format!("https://{}/api/v4/projects/{}", r.host, encode(&r.full())),
        ),
        Forge::BitbucketCloud => (
            BEARER,
            JSON,
            format!("https://{BITBUCKET_API_HOST}/2.0/repositories/{}", r.full()),
        ),
        Forge::BitbucketDc => (
            BEARER,
            JSON,
            format!(
                "https://{}/rest/api/1.0/projects/{}/repos/{}",
                r.host, r.owner, r.repo
            ),
        ),
        Forge::Forgejo => (
            BEARER,
            JSON,
            format!("https://{}/api/v1/repos/{}", r.host, r.full()),
        ),
    }
}

/// The header lines of a generated profile up to `endpoints:`.
fn profile_head(r: &GhRepo, comment: &str, what: &str, token_kind: &str, envs: &str) -> String {
    let full = r.full();
    let label = r.forge.label();
    let shown = if r.is_github() {
        full.clone()
    } else {
        r.shown()
    };
    format!(
        "# Generated by osh. {comment}\n\
id: {id}\n\
display_name: {label} {shown} (read/write)\n\
description: {what}\n\
category: source_control\n\
credentials:\n\
\x20 - name: api_token\n\
\x20   description: {token_kind} for {full}\n\
\x20   env_vars: [{envs}]\n\
\x20   required: true\n\
\x20   auth_style: bearer\n\
\x20   header_name: authorization\n\
discovery:\n\
\x20 credentials: [api_token]\n\
endpoints:\n",
        id = profile_id(r),
    )
}

/// Profile for one repo: read access on its provider plus `POST git-receive-pack` for that repo
/// only. GitHub's is shaped like `providers/github.yaml` so it attaches next to the other
/// GitHub profiles. Hosts and path segments were checked by `make`, so nothing in them can
/// break the YAML or widen a rule.
pub fn push_profile(r: &GhRepo) -> String {
    let [push_git, push_bare] = r.push_paths();
    let push = format!(
        "\x20     - allow: {{ method: POST, path: \"{push_git}\" }}\n\
\x20     - allow: {{ method: POST, path: \"{push_bare}\" }}\n"
    );
    let full = r.full();
    let host = &r.host;
    let curl_git =
        "binaries: [/usr/bin/curl, /usr/local/bin/curl, /usr/bin/git, /usr/local/bin/git]\n";
    match r.forge {
        Forge::GitHub => format!(
            "{head}\
\x20 - host: api.github.com\n\
\x20   port: 443\n\
\x20   protocol: rest\n\
\x20   access: read-only\n\
\x20   enforcement: enforce\n\
\x20 - host: api.github.com\n\
\x20   port: 443\n\
\x20   path: /graphql\n\
\x20   protocol: graphql\n\
\x20   access: read-only\n\
\x20   enforcement: enforce\n\
\x20 - host: github.com\n\
\x20   port: 443\n\
\x20   protocol: rest\n\
\x20   enforcement: enforce\n\
\x20   rules:\n\
\x20     - allow: {{ method: GET, path: \"**\" }}\n\
\x20     - allow: {{ method: HEAD, path: \"**\" }}\n\
\x20     - allow: {{ method: OPTIONS, path: \"**\" }}\n\
\x20     - allow: {{ method: POST, path: \"/**/git-upload-pack\" }}\n\
{push}\
binaries: [/usr/bin/gh, /usr/local/bin/gh, /usr/bin/git, /usr/local/bin/git]\n",
            head = profile_head(
                r,
                &format!("Read access as in providers/github.yaml, plus git push to {full} only."),
                &format!("GitHub API reads, git clone/fetch, and git push to {full}"),
                "Fine-grained GitHub token",
                "GITHUB_TOKEN, GH_TOKEN",
            ),
        ),
        Forge::Forgejo | Forge::GitLab => {
            let (api, comment, kind) = if r.forge == Forge::Forgejo {
                (
                    "/api/v1/**",
                    format!("Read access as in providers/forgejo.yaml, plus git push to {full} on {host} only."),
                    "Forgejo access token",
                )
            } else {
                (
                    "/api/v4/**",
                    format!("GitLab API reads and git clone/fetch on {host}, plus git push to {full} only."),
                    "GitLab project access token",
                )
            };
            format!(
                "{head}\
\x20 - host: {host}\n\
\x20   port: 443\n\
\x20   protocol: rest\n\
\x20   enforcement: enforce\n\
\x20   rules:\n\
\x20     - allow: {{ method: GET, path: \"{api}\" }}\n\
\x20     - allow: {{ method: HEAD, path: \"{api}\" }}\n\
\x20     - allow: {{ method: GET, path: \"/**/info/refs\" }}\n\
\x20     - allow: {{ method: POST, path: \"/**/git-upload-pack\" }}\n\
{push}\
{curl_git}",
                head = profile_head(
                    r,
                    &comment,
                    &format!(
                        "{} API reads, git clone/fetch, and git push to {full} on {host}",
                        r.forge.label()
                    ),
                    kind,
                    r.forge.token_env(),
                ),
            )
        }
        Forge::BitbucketCloud => format!(
            "{head}\
\x20 - host: {BITBUCKET_API_HOST}\n\
\x20   port: 443\n\
\x20   protocol: rest\n\
\x20   access: read-only\n\
\x20   enforcement: enforce\n\
\x20 - host: {host}\n\
\x20   port: 443\n\
\x20   protocol: rest\n\
\x20   enforcement: enforce\n\
\x20   rules:\n\
\x20     - allow: {{ method: GET, path: \"/**/info/refs\" }}\n\
\x20     - allow: {{ method: POST, path: \"/**/git-upload-pack\" }}\n\
{push}\
{curl_git}",
            head = profile_head(
                r,
                &format!("Bitbucket Cloud API reads and git clone/fetch, plus git push to {full} only."),
                &format!("Bitbucket API reads, git clone/fetch, and git push to {full}"),
                "Bitbucket repository access token",
                r.forge.token_env(),
            ),
        ),
        Forge::BitbucketDc => format!(
            "{head}\
\x20 - host: {host}\n\
\x20   port: 443\n\
\x20   protocol: rest\n\
\x20   enforcement: enforce\n\
\x20   rules:\n\
\x20     - allow: {{ method: GET, path: \"/rest/api/1.0/**\" }}\n\
\x20     - allow: {{ method: HEAD, path: \"/rest/api/1.0/**\" }}\n\
\x20     - allow: {{ method: GET, path: \"/scm/**/info/refs\" }}\n\
\x20     - allow: {{ method: POST, path: \"/scm/**/git-upload-pack\" }}\n\
{push}\
{curl_git}",
            head = profile_head(
                r,
                &format!("Bitbucket Data Center API reads and git clone/fetch on {host}, plus git push to {full} only."),
                &format!("Bitbucket Data Center API reads, git clone/fetch, and git push to {full} on {host}"),
                "Bitbucket repository HTTP access token",
                r.forge.token_env(),
            ),
        ),
    }
}

/// Whole seconds since the epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Epoch milliseconds `days` from `now`, the form `--credential-expires-at` takes.
pub fn expiry_ms(now: u64, days: u32) -> u64 {
    (now + u64::from(days) * 86_400) * 1000
}

/// Epoch seconds of a GitHub `github-authentication-token-expiration` header value, such as
/// `2026-12-05 12:00:00 UTC`. The token's real expiry: the user picks it on GitHub's page, so osh
/// must not assume the 30 days it pre-fills.
pub fn parse_token_expiry(v: &str) -> Option<u64> {
    let v = v.trim().trim_end_matches("UTC").trim();
    let (date, time) = v.split_once(' ')?;
    let mut d = date.split('-').map(|n| n.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|n| n.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    // Days since 1970-01-01 (Howard Hinnant's civil-date algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

/// Whole days left before `expiry_ms`; zero once it has passed.
pub fn days_left(now: u64, expiry_ms: u64) -> u64 {
    (expiry_ms / 1000).saturating_sub(now) / 86_400
}

/// A repo token osh created: when it expires and which repo it is for, kept on the host so
/// the Providers tab can warn and offer a renewal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tracked {
    pub gateway: String,
    pub provider: String,
    pub expiry_ms: u64,
    /// `GhRepo::spec`: `OWNER/REPO` for GitHub, `KEY:HOST/OWNER/REPO` otherwise.
    pub repo: String,
}

impl Tracked {
    /// The provider's name and the repo as shown on screen.
    pub fn shown(&self) -> (&'static str, String) {
        GhRepo::from_spec(&self.repo).map_or(("GitHub", self.repo.clone()), |r| {
            (r.forge.label(), r.shown())
        })
    }
}

fn parse_tracked(text: &str) -> Vec<Tracked> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some(Tracked {
                gateway: f.next()?.into(),
                provider: f.next()?.into(),
                expiry_ms: f.next()?.parse().ok()?,
                repo: f.next()?.into(),
            })
        })
        .collect()
}

fn format_tracked(all: &[Tracked]) -> String {
    all.iter()
        .map(|t| {
            format!(
                "{}\t{}\t{}\t{}\n",
                t.gateway, t.provider, t.expiry_ms, t.repo
            )
        })
        .collect()
}

/// Where tracked tokens live: `$XDG_STATE_HOME/osh/tokens`, else `~/.local/state/osh/tokens`.
pub fn tracked_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return None;
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("osh/tokens"))
}

/// Where the answers to "Which git provider hosts HOST?" live, next to the tracked tokens.
pub fn hosts_path() -> Option<std::path::PathBuf> {
    tracked_path().map(|p| p.with_file_name("git-hosts"))
}

/// The file an earlier osh kept "is HOST a Forgejo instance?" answers in, next to `file`. Its
/// "yes" hosts are read as Forgejo until the user is asked again; its "no" hosts are asked
/// again, since a "no" then may mean GitLab or Bitbucket now.
fn legacy_hosts_path(file: &std::path::Path) -> std::path::PathBuf {
    file.with_file_name("forgejo-hosts")
}

/// What the user said about one host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostEntry {
    pub host: String,
    /// `None`: "None / other", no repo access.
    pub forge: Option<Forge>,
    /// The ssh port of the remote the answer came from, when not 22: sandbox git rewrites
    /// `ssh://git@HOST:PORT/` to https too.
    pub ssh_port: Option<u16>,
}

/// Every remembered host, the newest answer per host, old Forgejo "yes" answers included.
pub fn load_hosts(file: Option<&std::path::Path>) -> Vec<HostEntry> {
    let Some(file) = file else { return vec![] };
    let mut out: Vec<HostEntry> = Vec::new();
    let text = std::fs::read_to_string(file).unwrap_or_default();
    for l in text.lines() {
        let mut f = l.split('\t');
        let (Some(host), Some(key)) = (f.next(), f.next()) else {
            continue;
        };
        if !valid_host(host) {
            continue;
        }
        out.retain(|e| e.host != host);
        out.push(HostEntry {
            host: host.into(),
            forge: Forge::from_key(key, host),
            ssh_port: f.next().and_then(|p| p.parse().ok()),
        });
    }
    let legacy = std::fs::read_to_string(legacy_hosts_path(file)).unwrap_or_default();
    for (host, a) in legacy.lines().filter_map(|l| l.split_once('\t')) {
        if a == "yes" && valid_host(host) && !out.iter().any(|e| e.host == host) {
            out.push(HostEntry {
                host: host.into(),
                forge: Some(Forge::Forgejo),
                ssh_port: None,
            });
        }
    }
    out
}

/// The remembered answer for `host`, if the user was asked.
pub fn host_entry(file: Option<&std::path::Path>, host: &str) -> Option<HostEntry> {
    load_hosts(file).into_iter().find(|e| e.host == host)
}

/// Remember the provider of `host` (`None`: no repo access), so it is never asked again.
pub fn remember_host(
    file: Option<&std::path::Path>,
    host: &str,
    forge: Option<Forge>,
    ssh_port: Option<u16>,
) {
    let Some(file) = file else { return };
    if !valid_host(host) {
        return;
    }
    let mut text: String = std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.split('\t').next() != Some(host))
        .map(|l| format!("{l}\n"))
        .collect();
    let key = forge.map_or("none", Forge::key);
    match ssh_port {
        Some(p) => text.push_str(&format!("{host}\t{key}\t{p}\n")),
        None => text.push_str(&format!("{host}\t{key}\n")),
    }
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, text);
}

/// Drop the answer for `host` (an older Forgejo answer too), so osh asks again next time.
pub fn forget_host(file: Option<&std::path::Path>, host: &str) {
    let Some(file) = file else { return };
    for f in [file.to_path_buf(), legacy_hosts_path(file)] {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        let kept: String = text
            .lines()
            .filter(|l| l.split('\t').next() != Some(host))
            .map(|l| format!("{l}\n"))
            .collect();
        if kept != text {
            let _ = std::fs::write(&f, kept);
        }
    }
}

pub fn load_tracked(file: Option<&std::path::Path>) -> Vec<Tracked> {
    file.and_then(|f| std::fs::read_to_string(f).ok())
        .map(|t| parse_tracked(&t))
        .unwrap_or_default()
}

/// Add or replace the entry for `t.gateway` + `t.provider`.
pub fn track(file: Option<&std::path::Path>, t: Tracked) {
    let Some(file) = file else { return };
    let mut all = load_tracked(Some(file));
    all.retain(|o| !(o.gateway == t.gateway && o.provider == t.provider));
    all.push(t);
    save_tracked(file, &all);
}

pub fn untrack(file: Option<&std::path::Path>, gateway: &str, provider: &str) {
    let Some(file) = file else { return };
    let mut all = load_tracked(Some(file));
    let n = all.len();
    all.retain(|o| !(o.gateway == gateway && o.provider == provider));
    if all.len() != n {
        save_tracked(file, &all);
    }
}

fn save_tracked(file: &std::path::Path, all: &[Tracked]) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, format_tracked(all));
}

/// "expires in 12d", "expires today" or "expired" for a token expiring at `expiry_ms`.
pub fn expiry_note(now: u64, expiry_ms: u64) -> String {
    if expiry_ms / 1000 <= now {
        "expired".into()
    } else {
        match days_left(now, expiry_ms) {
            0 => "expires today".into(),
            d => format!("expires in {d}d"),
        }
    }
}

/// Tokens of `providers` (names on the gateway) that expired or expire within `WARN_DAYS`,
/// soonest first.
pub fn expiring<'a>(
    all: &'a [Tracked],
    gateway: &str,
    providers: &[String],
    now: u64,
) -> Vec<&'a Tracked> {
    let mut out: Vec<&Tracked> = all
        .iter()
        .filter(|t| {
            t.gateway == gateway
                && providers.contains(&t.provider)
                && days_left(now, t.expiry_ms) <= WARN_DAYS
        })
        .collect();
    out.sort_by_key(|t| t.expiry_ms);
    out
}

/// One line about the soonest of `list`, which must not be empty.
pub fn banner(list: &[&Tracked], now: u64) -> String {
    let t = list[0];
    let more = match list.len() {
        1 => String::new(),
        n => format!(" (+{} more)", n - 1),
    };
    let (label, repo) = t.shown();
    format!(
        "{label} token {} ({repo}) {}{more}: renew with u on the Providers tab",
        t.provider,
        expiry_note(now, t.expiry_ms)
    )
}

/// GitHub's part of `git_exports`, unchanged since it was the only provider: `count` is the
/// total number of `GIT_CONFIG_*` keys.
fn github_exports(count: usize) -> String {
    format!(
        r#"export GIT_CONFIG_COUNT={count} \
GIT_CONFIG_KEY_0=url.https://github.com/.insteadOf GIT_CONFIG_VALUE_0=git@github.com: \
GIT_CONFIG_KEY_1=url.https://github.com/.insteadOf GIT_CONFIG_VALUE_1=ssh://git@github.com/ \
GIT_CONFIG_KEY_2=credential.https://github.com.helper \
GIT_CONFIG_VALUE_2='!f() {{ [ "$1" = get ] || exit 0; t=${{GITHUB_TOKEN:-$GH_TOKEN}}; [ -n "$t" ] && echo username=x-access-token && echo password=$t; }}; f'"#
    )
}

/// Environment for sandbox git, set per process so no repo config changes. Remotes of GitHub
/// and of every host remembered as GitLab, Bitbucket or Forgejo use HTTPS whichever way
/// `origin` is spelled (the providers only reach port 443; a Data Center ssh remote
/// `ssh://git@HOST:PORT/P/R` becomes `https://HOST/scm/P/R`), and a credential helper per host
/// hands git the provider's username and token placeholder. Git sends nothing on its own,
/// GitHub (and per their docs GitLab and Bitbucket Cloud) reject a Bearer token for git, and
/// the proxy swaps the secret into the Basic header the client sends, so the helper supplies
/// the placeholder. Hosts are checked with `valid_host` and every non-GitHub word is single
/// quoted, so nothing in them can reach the shell.
pub fn git_exports(hosts: &[HostEntry]) -> String {
    let mut extra: Vec<(String, String)> = Vec::new();
    for e in hosts {
        let Some(forge) = e.forge else { continue };
        if forge == Forge::GitHub || e.host == GITHUB_HOST || !valid_host(&e.host) {
            continue;
        }
        let h = &e.host;
        let dc = forge == Forge::BitbucketDc;
        let key = if dc {
            format!("'url.https://{h}/scm/.insteadOf'")
        } else {
            format!("'url.https://{h}/.insteadOf'")
        };
        if !dc {
            extra.push((key.clone(), format!("'git@{h}:'")));
        }
        extra.push((key.clone(), format!("'ssh://git@{h}/'")));
        let mut ports: Vec<u16> = Vec::new();
        if dc {
            ports.push(BITBUCKET_DC_SSH_PORT);
        }
        ports.extend(e.ssh_port.filter(|p| !ports.contains(p)));
        for p in ports {
            extra.push((key.clone(), format!("'ssh://git@{h}:{p}/'")));
        }
        let var = forge.token_env();
        extra.push((
            format!("'credential.https://{h}.helper'"),
            format!(
                "'!f() {{ [ \"$1\" = get ] || exit 0; [ -n \"${var}\" ] && echo username={} && echo password=${var}; }}; f'",
                forge.git_user()
            ),
        ));
    }
    let mut out = github_exports(3 + extra.len());
    for (i, (k, v)) in extra.iter().enumerate() {
        let n = i + 3;
        out.push_str(&format!(
            " \\\nGIT_CONFIG_KEY_{n}={k} GIT_CONFIG_VALUE_{n}={v}"
        ));
    }
    out.push_str("; ");
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn token_name_matches_the_prefilled_page_and_stays_short() {
        let r = parse_remote("https://github.com/acme/app").unwrap();
        assert_eq!(token_name("box-app"), "osh-box-app");
        assert!(token_url(&r, "box-app").contains("name=osh-box-app&"));
        assert!(token_name(&"x".repeat(80)).len() <= 40);
    }

    use super::*;

    fn repo(o: &str, r: &str) -> GhRepo {
        GhRepo {
            forge: Forge::GitHub,
            host: GITHUB_HOST.into(),
            owner: o.into(),
            repo: r.into(),
        }
    }

    fn other(forge: Forge, host: &str, o: &str, r: &str) -> GhRepo {
        GhRepo {
            forge,
            host: host.into(),
            owner: o.into(),
            repo: r.into(),
        }
    }

    #[test]
    fn parses_ssh_https_and_ssh_url_remotes() {
        let want = Some(repo("Acme", "app"));
        for u in [
            "git@github.com:Acme/app.git",
            "git@github.com:Acme/app",
            "ssh://git@github.com/Acme/app.git",
            "https://github.com/Acme/app.git",
            "https://github.com/Acme/app/",
            "  https://github.com/Acme/app\n",
        ] {
            assert_eq!(parse_remote(u), want, "{u}");
        }
    }

    #[test]
    fn rejects_other_hosts_and_odd_paths() {
        for u in [
            "git@example.com:a/b.git",
            "https://github.com.evil.io/a/b",
            "https://github.com/a",
            "https://github.com/a/b/c",
            "https://github.com/../b",
            "https://github.com/a..b/c",
            "https://github.com/~a/b",
            "git@github.com:a/b c.git",
            "git@github.com:a/\"b",
            "git@github.com:a/*",
            "",
        ] {
            assert_eq!(parse_remote(u), None, "{u}");
        }
    }

    #[test]
    fn names_are_safe_and_follow_the_sandbox_and_repo() {
        let r = repo("Acme", "My_Repo.rs");
        assert_eq!(slug("  My_Repo.rs "), "my-repo-rs");
        assert_eq!(profile_id(&r), "gh-acme-my-repo-rs");
        assert_eq!(provider_name("Dev Box", &r), "github-dev-box-my-repo-rs");
        assert_eq!(shared_provider_name(&r), "github-my-repo-rs");
    }

    #[test]
    fn token_url_prefills_owner_expiry_and_permissions_without_a_repo() {
        let u = token_url(&repo("acme", "app"), "dev-app");
        assert!(u.starts_with("https://github.com/settings/personal-access-tokens/new?"));
        assert!(u.contains("name=osh-dev-app&"));
        assert!(u.contains("target_name=acme&"));
        assert!(u.contains("expires_in=30&"));
        assert!(u.contains("contents=write") && u.contains("pull_requests=write"));
        assert!(u.contains("workflows=write"));
        assert!(u.contains("description=osh%20sandbox%20access%20to%20acme%2Fapp"));
    }

    #[test]
    fn token_name_stays_within_githubs_limit() {
        let u = token_url(&repo("a", "b"), &"x".repeat(80));
        let name = u.split("name=").nth(1).unwrap().split('&').next().unwrap();
        assert!(name.len() <= 40, "{name}");
    }

    #[test]
    fn steps_name_the_repo_the_url_and_the_org_caveat() {
        let r = repo("acme", "app");
        let s = steps(&r, "https://example.test/x");
        assert!(s.contains("Only select repositories, then acme/app"));
        assert!(s.contains("https://example.test/x"));
        assert!(s.contains("30 days"));
        assert!(s.contains("approve"));
    }

    #[test]
    fn profile_grants_push_to_that_repo_only() {
        let y = push_profile(&repo("acme", "app"));
        assert!(y.starts_with("# Generated by osh. Read access as in providers/github.yaml"));
        assert!(y.contains("id: gh-acme-app\n"));
        assert!(y.contains("display_name: GitHub acme/app (read/write)\n"));
        assert!(y.contains("/acme/app.git/git-receive-pack"));
        assert!(y.contains("/acme/app/git-receive-pack"));
        assert!(!y.contains("/**/git-receive-pack"));
        assert!(y.contains("/**/git-upload-pack"));
        assert!(y.contains("env_vars: [GITHUB_TOKEN, GH_TOKEN]"));
        assert!(y.contains("access: read-only"));
        assert!(y.ends_with(
            "binaries: [/usr/bin/gh, /usr/local/bin/gh, /usr/bin/git, /usr/local/bin/git]\n"
        ));
    }

    #[test]
    fn the_github_expiry_header_is_read_as_utc_epoch_seconds() {
        assert_eq!(parse_token_expiry("1970-01-02 00:00:01 UTC"), Some(86_401));
        assert_eq!(
            parse_token_expiry("2026-11-10 12:00:00 UTC"),
            Some(1_794_312_000)
        );
        assert_eq!(
            parse_token_expiry("2024-02-29 00:00:00 UTC"),
            Some(1_709_164_800)
        );
        assert_eq!(parse_token_expiry("never"), None);
        assert_eq!(parse_token_expiry("2026-13-01 00:00:00 UTC"), None);
    }

    #[test]
    fn expiry_math_round_trips() {
        let ms = expiry_ms(1_000, 30);
        assert_eq!(ms, (1_000 + 30 * 86_400) * 1000);
        assert_eq!(days_left(1_000, ms), 30);
        assert_eq!(days_left(1_000 + 25 * 86_400 + 5, ms), 4);
        assert_eq!(days_left(u64::MAX / 2, ms), 0);
    }

    #[test]
    fn tracked_tokens_round_trip_and_replace_by_gateway_and_provider() {
        let dir = std::env::temp_dir().join(format!("osh-ghrepo-{}", now_secs()));
        let file = dir.join("tokens");
        let t = |g: &str, p: &str, ms: u64| Tracked {
            gateway: g.into(),
            provider: p.into(),
            expiry_ms: ms,
            repo: "acme/app".into(),
        };
        track(Some(&file), t("vm", "github-a-app", 1));
        track(Some(&file), t("vm", "github-b-app", 2));
        track(Some(&file), t("docker", "github-a-app", 3));
        track(Some(&file), t("vm", "github-a-app", 4));
        let all = load_tracked(Some(&file));
        assert_eq!(all.len(), 3);
        assert!(all.contains(&t("vm", "github-a-app", 4)));
        untrack(Some(&file), "vm", "github-b-app");
        assert_eq!(load_tracked(Some(&file)).len(), 2);
        assert!(load_tracked(None).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn expiring_lists_only_near_present_tokens_of_this_gateway_soonest_first() {
        let now = 10_000_000;
        let t = |g: &str, p: &str, days: i64| Tracked {
            gateway: g.into(),
            provider: p.into(),
            expiry_ms: ((now as i64 + days * 86_400) * 1000) as u64,
            repo: "acme/app".into(),
        };
        let all = vec![
            t("vm", "a", 6),
            t("vm", "b", -2),
            t("vm", "far", 20),
            t("vm", "gone", 1),
            t("other", "c", 1),
        ];
        let names: Vec<String> = ["a", "b", "far", "c"].map(String::from).to_vec();
        let list = expiring(&all, "vm", &names, now);
        let got: Vec<&str> = list.iter().map(|t| t.provider.as_str()).collect();
        assert_eq!(got, ["b", "a"]);
        let b = banner(&list, now);
        assert!(b.starts_with("GitHub token b (acme/app)"));
        assert!(b.contains("expired") && b.contains("(+1 more)"));
        assert!(banner(&list[1..], now).contains("expires in 6d"));
    }

    #[test]
    fn banner_names_the_provider_of_a_non_github_token() {
        let r = other(Forge::GitLab, "gitlab.com", "grp/sub", "app");
        let t = Tracked {
            gateway: "vm".into(),
            provider: "gitlab-app".into(),
            expiry_ms: expiry_ms(1_000, 3),
            repo: r.spec(),
        };
        assert_eq!(t.shown(), ("GitLab", "gitlab.com/grp/sub/app".to_string()));
        assert!(banner(&[&t], 1_000)
            .starts_with("GitLab token gitlab-app (gitlab.com/grp/sub/app) expires in 3d"));
    }

    #[test]
    fn expiry_note_reads_naturally() {
        let now = 1_000;
        assert_eq!(expiry_note(now, expiry_ms(now, 12)), "expires in 12d");
        assert_eq!(expiry_note(now, (now + 3600) * 1000), "expires today");
        assert_eq!(expiry_note(now, (now - 5) * 1000), "expired");
    }

    const GITHUB_EXPORTS_BEFORE: &str = r#"export GIT_CONFIG_COUNT=3 \
GIT_CONFIG_KEY_0=url.https://github.com/.insteadOf GIT_CONFIG_VALUE_0=git@github.com: \
GIT_CONFIG_KEY_1=url.https://github.com/.insteadOf GIT_CONFIG_VALUE_1=ssh://git@github.com/ \
GIT_CONFIG_KEY_2=credential.https://github.com.helper \
GIT_CONFIG_VALUE_2='!f() { [ "$1" = get ] || exit 0; t=${GITHUB_TOKEN:-$GH_TOKEN}; [ -n "$t" ] && echo username=x-access-token && echo password=$t; }; f'; "#;

    #[test]
    fn git_exports_rewrite_remotes_and_hand_git_the_token() {
        let e = git_exports(&[]);
        assert_eq!(e, GITHUB_EXPORTS_BEFORE, "GitHub alone is unchanged");
        assert!(e.ends_with("; "), "must chain into the next command");
        // The helper is single-quoted: nothing in it may expand when the shell exports it.
        let helper = e.split("GIT_CONFIG_VALUE_2=").nth(1).unwrap();
        assert!(helper.starts_with('\'') && helper.trim_end_matches("; ").ends_with('\''));
        assert_eq!(helper.matches('\'').count(), 2);
        // A remembered GitHub, "none" or bad host adds nothing.
        let skip = [
            HostEntry {
                host: "github.com".into(),
                forge: Some(Forge::GitHub),
                ssh_port: None,
            },
            HostEntry {
                host: "x.test".into(),
                forge: None,
                ssh_port: None,
            },
            HostEntry {
                host: "a'b".into(),
                forge: Some(Forge::GitLab),
                ssh_port: None,
            },
        ];
        assert_eq!(git_exports(&skip), GITHUB_EXPORTS_BEFORE);
    }

    /// The exports as (key, value) pairs, after checking the count matches.
    fn pairs(e: &str) -> Vec<(String, String)> {
        let count: usize = e
            .split("GIT_CONFIG_COUNT=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let out: Vec<(String, String)> = (0..count)
            .map(|i| {
                let k = e
                    .split(&format!("GIT_CONFIG_KEY_{i}="))
                    .nth(1)
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap();
                let v = e
                    .split(&format!("GIT_CONFIG_VALUE_{i}="))
                    .nth(1)
                    .unwrap()
                    .split(" \\\n")
                    .next()
                    .unwrap()
                    .trim_end_matches("; ");
                (k.to_string(), v.to_string())
            })
            .collect();
        assert!(!e.contains(&format!("GIT_CONFIG_KEY_{count}=")));
        out
    }

    #[test]
    fn git_exports_cover_each_remembered_provider_host() {
        let hosts = [
            HostEntry {
                host: "example.com".into(),
                forge: Some(Forge::GitLab),
                ssh_port: Some(2222),
            },
            HostEntry {
                host: "bitbucket.org".into(),
                forge: Some(Forge::BitbucketCloud),
                ssh_port: None,
            },
            HostEntry {
                host: "example.net".into(),
                forge: Some(Forge::BitbucketDc),
                ssh_port: Some(7999),
            },
            HostEntry {
                host: "codeberg.org".into(),
                forge: Some(Forge::Forgejo),
                ssh_port: None,
            },
        ];
        let e = git_exports(&hosts);
        let p = pairs(&e);
        // 3 GitHub + GitLab 4 (scp, ssh, ssh:2222, helper) + Cloud 3 + DC 3 (ssh, ssh:7999,
        // helper) + Forgejo 3.
        assert_eq!(p.len(), 16);
        let has = |k: &str, v: &str| p.iter().any(|(a, b)| a == k && b == v);
        let gl = "'url.https://example.com/.insteadOf'";
        assert!(has(gl, "'git@example.com:'"));
        assert!(has(gl, "'ssh://git@example.com/'"));
        assert!(has(gl, "'ssh://git@example.com:2222/'"));
        let dc = "'url.https://example.net/scm/.insteadOf'";
        assert!(has(dc, "'ssh://git@example.net:7999/'"));
        assert!(has(dc, "'ssh://git@example.net/'"));
        assert!(!p.iter().any(|(_, v)| v == "'git@example.net:'"));
        let helper = |h: &str| {
            p.iter()
                .find(|(k, _)| k == &format!("'credential.https://{h}.helper'"))
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        let g = helper("example.com");
        assert!(g.contains("username=oauth2") && g.contains("password=$GITLAB_TOKEN"));
        let b = helper("bitbucket.org");
        assert!(b.contains("username=x-token-auth") && b.contains("password=$BITBUCKET_TOKEN"));
        assert!(helper("example.net").contains("username=x-token-auth"));
        let f = helper("codeberg.org");
        assert!(f.contains("username=oauth2") && f.contains("$FORGEJO_TOKEN"));
        // Every non-GitHub word is one single-quoted shell word.
        for (k, v) in &p[3..] {
            for w in [k, v] {
                assert!(w.starts_with('\'') && w.ends_with('\''), "{w}");
                assert_eq!(w.matches('\'').count(), 2, "{w}");
            }
        }
        assert!(e.ends_with("; "));
    }

    #[test]
    fn parse_url_reads_other_hosts_and_refuses_https_ports() {
        let fj = |r: Option<Remote>| r.and_then(|r| r.repo(Forge::Forgejo));
        let want = Some(other(Forge::Forgejo, "example.org", "acme", "app"));
        for u in [
            "git@example.org:acme/app.git",
            "ssh://git@example.org/acme/app.git",
            "ssh://git@example.org:2222/acme/app",
            "https://Example.org/acme/app.git",
            "https://example.org/acme/app/",
            "https://user@example.org/acme/app",
        ] {
            assert_eq!(fj(parse_url(u)), want, "{u}");
        }
        for u in [
            "https://example.org:3000/acme/app",
            "https://example.org/acme",
            "ssh://git@example.org:x/acme/app",
            "/home/me/repo",
            "ftp://example.org/a/b",
            "",
        ] {
            assert_eq!(fj(parse_url(u)), None, "{u}");
        }
        assert_eq!(
            parse_url("ssh://git@example.invalid:2222/a/b")
                .unwrap()
                .ssh_port,
            Some(2222)
        );
        assert_eq!(
            parse_url("ssh://git@example.invalid:22/a/b")
                .unwrap()
                .ssh_port,
            None
        );
        assert_eq!(parse_remote("git@example.org:acme/app.git"), None);
        assert!(parse_remote("git@github.com:a/b").unwrap().is_github());
    }

    #[test]
    fn gitlab_remotes_keep_nested_groups() {
        let want = Some(other(Forge::GitLab, "gitlab.com", "grp/sub/team", "app"));
        for u in [
            "https://gitlab.com/grp/sub/team/app",
            "https://gitlab.com/grp/sub/team/app.git",
        ] {
            assert_eq!(parse_url(u).unwrap().repo(Forge::GitLab), want, "{u}");
        }
        let one = parse_url("https://gitlab.com/a/b").unwrap();
        assert_eq!(one.repo(Forge::GitLab).unwrap().owner, "a");
        // Only GitLab takes nested groups.
        let deep = parse_url("https://example.org/a/b/c").unwrap();
        assert_eq!(deep.repo(Forge::Forgejo), None);
        assert_eq!(deep.repo(Forge::GitHub), None);
    }

    #[test]
    fn bitbucket_remotes_split_cloud_from_data_center() {
        let cloud = Some(other(Forge::BitbucketCloud, "bitbucket.org", "ws", "app"));
        for u in [
            "https://bitbucket.org/ws/app",
            "https://bitbucket.org/ws/app.git/",
        ] {
            assert_eq!(
                parse_url(u).unwrap().repo(Forge::BitbucketCloud),
                cloud,
                "{u}"
            );
        }
        let dc = Some(other(Forge::BitbucketDc, "example.net", "PROJ", "app"));
        for u in [
            "https://example.net/scm/PROJ/app.git",
            "https://me@example.net/scm/PROJ/app",
            "ssh://git@example.net:7999/PROJ/app.git",
            "ssh://git@example.net:2200/PROJ/app.git",
        ] {
            assert_eq!(parse_url(u).unwrap().repo(Forge::BitbucketDc), dc, "{u}");
        }
        let personal = parse_url("https://example.net/scm/~me/app.git").unwrap();
        assert_eq!(personal.repo(Forge::BitbucketDc).unwrap().owner, "~me");
        // https without /scm/ (a context path, or not Data Center) is not supported.
        for u in [
            "https://example.net/PROJ/app.git",
            "https://example.net/bitbucket/scm/PROJ/app.git",
        ] {
            assert_eq!(parse_url(u).unwrap().repo(Forge::BitbucketDc), None, "{u}");
        }
        // Cloud is bitbucket.org only, Data Center never.
        let cloud_url = parse_url("https://bitbucket.org/ws/app").unwrap();
        assert_eq!(cloud_url.repo(Forge::BitbucketDc), None);
        let dc_ssh = parse_url("ssh://git@example.net:7999/PROJ/app.git").unwrap();
        assert_eq!(dc_ssh.repo(Forge::BitbucketCloud), None);
        assert_eq!(
            Forge::from_key("bitbucket", "bitbucket.org"),
            Some(Forge::BitbucketCloud)
        );
        assert_eq!(
            Forge::from_key("bitbucket", "example.net"),
            Some(Forge::BitbucketDc)
        );
    }

    #[test]
    fn specs_round_trip_for_every_provider() {
        for r in [
            repo("acme", "app"),
            other(Forge::GitLab, "gitlab.com", "a/b/c", "app"),
            other(Forge::BitbucketCloud, "bitbucket.org", "ws", "app"),
            other(Forge::BitbucketDc, "example.net", "~me", "app"),
            other(Forge::Forgejo, "codeberg.org", "acme", "app"),
        ] {
            assert_eq!(
                GhRepo::from_spec(&r.spec()),
                Some(r.clone()),
                "{}",
                r.spec()
            );
        }
        assert_eq!(repo("acme", "app").spec(), "acme/app");
        assert_eq!(GhRepo::from_spec("gitlab:bad host/a/b"), None);
        assert_eq!(GhRepo::from_spec("nope:example.invalid/a/b"), None);
    }

    #[test]
    fn the_likely_provider_comes_first_and_none_last() {
        let first = |h: &str| provider_choices(h)[0].clone();
        assert_eq!(first("github.com"), "GitHub");
        assert_eq!(first("gitlab.com"), "GitLab");
        assert_eq!(first("bitbucket.org"), "Bitbucket");
        assert_eq!(first("codeberg.org"), "Forgejo");
        assert_eq!(first("example.org"), "GitHub");
        let c = provider_choices("codeberg.org");
        assert_eq!(
            c,
            ["Forgejo", "GitHub", "GitLab", "Bitbucket", NO_PROVIDER_ITEM]
        );
        assert_eq!(chosen_forge("GitLab", "x.test"), Some(Forge::GitLab));
        assert_eq!(
            chosen_forge("Bitbucket", "x.test"),
            Some(Forge::BitbucketDc)
        );
        assert_eq!(
            chosen_forge("Bitbucket", "bitbucket.org"),
            Some(Forge::BitbucketCloud)
        );
        assert_eq!(chosen_forge(NO_PROVIDER_ITEM, "x.test"), None);
    }

    #[test]
    fn provider_names_differ_by_provider() {
        let fj = other(Forge::Forgejo, "example.org", "Acme", "My_Repo");
        assert_eq!(profile_id(&fj), "fj-example-org-acme-my-repo");
        assert_eq!(provider_name("Dev", &fj), "forgejo-dev-my-repo");
        assert_eq!(shared_provider_name(&fj), "forgejo-my-repo");
        assert_eq!(token_env(&fj), "FORGEJO_TOKEN");
        assert_eq!(fj.shown(), "example.org/Acme/My_Repo");
        let gl = other(Forge::GitLab, "gitlab.com", "a/b", "app");
        assert_eq!(profile_id(&gl), "gl-gitlab-com-a-b-app");
        assert_eq!(provider_name("dev", &gl), "gitlab-dev-app");
        assert_eq!(token_env(&gl), "GITLAB_TOKEN");
        let dc = other(Forge::BitbucketDc, "example.net", "PROJ", "app");
        assert_eq!(profile_id(&dc), "bb-example-net-proj-app");
        assert_eq!(shared_provider_name(&dc), "bitbucket-app");
        assert_eq!(token_env(&dc), "BITBUCKET_TOKEN");
        assert_eq!(
            sandbox_prefix(Forge::BitbucketCloud, "Dev"),
            "bitbucket-dev-"
        );
    }

    #[test]
    fn forgejo_steps_have_no_expiry_and_name_scope_and_page() {
        let r = other(Forge::Forgejo, "example.org", "acme", "app");
        let url = token_url(&r, "dev-app");
        assert_eq!(
            url,
            "https://example.org/user/settings/applications/tokens/new"
        );
        let s = steps_for(&r, &url, "dev-app");
        assert!(s.contains(&url) && s.contains("osh-dev-app"));
        assert!(s.contains("write:repository"));
        assert!(s.contains("Specific repositories, then acme/app"));
        assert!(!s.contains("30") && !s.to_lowercase().contains("expires in"));
        assert!(s.contains("do not expire"));
        assert!(s.contains("https://example.org/user/settings/applications"));
    }

    #[test]
    fn gitlab_steps_and_url_name_role_scopes_expiry_and_the_free_tier() {
        let r = other(Forge::GitLab, "gitlab.com", "grp/sub", "app");
        let url = token_url(&r, "dev-app");
        assert_eq!(
            url,
            "https://gitlab.com/grp/sub/app/-/settings/access_tokens?name=osh-dev-app&scopes=read_api,write_repository"
        );
        let s = steps_for(&r, &url, "dev-app");
        assert!(s.contains("project access token for grp/sub/app on gitlab.com"));
        assert!(s.contains("Settings > Access tokens"));
        assert!(s.contains("Developer") && s.contains("osh-dev-app"));
        assert!(s.contains("read_api and write_repository"));
        assert!(s.contains("30 days"));
        assert!(s.contains("Premium or Ultimate") && s.contains("personal access token"));
        assert!(s.contains("every project your account can"));
        assert_eq!(
            tokens_page(&r),
            "https://gitlab.com/grp/sub/app/-/settings/access_tokens"
        );
    }

    #[test]
    fn bitbucket_steps_name_permissions_and_the_menu_path() {
        let c = other(Forge::BitbucketCloud, "bitbucket.org", "ws", "app");
        let url = token_url(&c, "app");
        assert_eq!(url, "https://bitbucket.org/ws/app/admin/access-tokens");
        let s = steps_for(&c, &url, "app");
        assert!(s.contains("repository access token for ws/app"));
        assert!(s.contains("Repository settings > Security > Access tokens"));
        assert!(s.contains("Repositories Read and Write") && s.contains("30 days"));
        let d = other(Forge::BitbucketDc, "example.net", "PROJ", "app");
        let url = token_url(&d, "app");
        assert_eq!(url, "https://example.net/projects/PROJ/repos/app/settings");
        let s = steps_for(&d, &url, "app");
        assert!(s.contains("HTTP access tokens > Create token"));
        assert!(s.contains("Repository write") && s.contains("30 days"));
        assert!(tokens_page(&d).contains("HTTP access tokens"));
    }

    #[test]
    fn check_requests_hit_a_read_only_repo_endpoint_per_provider() {
        let (h, _, u) = check_request(&repo("acme", "app"));
        assert_eq!(
            (h, u.as_str()),
            (
                "Authorization: Bearer ",
                "https://api.github.com/repos/acme/app"
            )
        );
        let (h, _, u) = check_request(&other(Forge::GitLab, "gitlab.com", "a/b", "app"));
        assert_eq!(
            (h, u.as_str()),
            (
                "PRIVATE-TOKEN: ",
                "https://gitlab.com/api/v4/projects/a%2Fb%2Fapp"
            )
        );
        let (h, _, u) = check_request(&other(Forge::BitbucketCloud, "bitbucket.org", "ws", "app"));
        assert_eq!(
            (h, u.as_str()),
            (
                "Authorization: Bearer ",
                "https://api.bitbucket.org/2.0/repositories/ws/app"
            )
        );
        let (_, _, u) = check_request(&other(Forge::BitbucketDc, "example.net", "PROJ", "app"));
        assert_eq!(
            u,
            "https://example.net/rest/api/1.0/projects/PROJ/repos/app"
        );
        let (_, _, u) = check_request(&other(Forge::Forgejo, "example.org", "a", "b"));
        assert_eq!(u, "https://example.org/api/v1/repos/a/b");
    }

    /// The `- host:` lines and every rule line of a profile.
    fn hosts_of(y: &str) -> Vec<&str> {
        y.lines()
            .filter_map(|l| l.trim().strip_prefix("- host: "))
            .collect()
    }

    fn assert_safe(y: &str) {
        assert!(!y.contains("/**/git-receive-pack"), "{y}");
        assert!(!y.contains("method: PUT") && !y.contains("method: DELETE"));
        assert!(!y.contains("method: PATCH") && !y.contains("/usr/bin/gh"));
        assert!(hosts_of(y).iter().all(|h| !h.contains('*')));
        assert_eq!(y.matches("git-receive-pack").count(), 2, "{y}");
    }

    #[test]
    fn forgejo_profile_grants_push_to_that_repo_on_that_host_only() {
        let y = push_profile(&other(Forge::Forgejo, "example.org", "acme", "app"));
        assert!(y.contains("id: fj-example-org-acme-app\n"));
        assert!(y.contains("- host: example.org\n"));
        assert!(y.contains("port: 443"));
        assert!(y.contains("\"/acme/app.git/git-receive-pack\""));
        assert!(y.contains("\"/acme/app/git-receive-pack\""));
        assert!(y.contains("\"/**/git-upload-pack\"") && y.contains("\"/api/v1/**\""));
        assert!(y.contains("env_vars: [FORGEJO_TOKEN]") && y.contains("auth_style: bearer"));
        assert!(!y.contains("GITHUB"));
        assert_safe(&y);
    }

    #[test]
    fn gitlab_profile_reads_the_api_and_pushes_to_the_nested_repo_only() {
        let y = push_profile(&other(Forge::GitLab, "example.com", "a/b", "app"));
        assert!(y.contains("id: gl-example-com-a-b-app\n"));
        assert_eq!(hosts_of(&y), ["example.com"]);
        assert!(y.contains("{ method: GET, path: \"/api/v4/**\" }"));
        assert!(y.contains("\"/a/b/app.git/git-receive-pack\""));
        assert!(y.contains("\"/a/b/app/git-receive-pack\""));
        assert!(y.contains("env_vars: [GITLAB_TOKEN]"));
        assert!(y.ends_with("/usr/bin/git, /usr/local/bin/git]\n"));
        assert_safe(&y);
    }

    #[test]
    fn bitbucket_cloud_profile_reads_the_api_host_and_pushes_on_the_git_host() {
        let y = push_profile(&other(Forge::BitbucketCloud, "bitbucket.org", "ws", "app"));
        assert_eq!(hosts_of(&y), ["api.bitbucket.org", "bitbucket.org"]);
        let api = y.split("- host: bitbucket.org").next().unwrap();
        assert!(api.contains("access: read-only") && !api.contains("rules:"));
        assert!(y.contains("\"/ws/app.git/git-receive-pack\""));
        assert!(y.contains("env_vars: [BITBUCKET_TOKEN]"));
        assert_safe(&y);
    }

    #[test]
    fn bitbucket_dc_profile_keeps_everything_under_scm_and_rest() {
        let y = push_profile(&other(Forge::BitbucketDc, "example.net", "PROJ", "app"));
        assert_eq!(hosts_of(&y), ["example.net"]);
        assert!(y.contains("\"/rest/api/1.0/**\""));
        assert!(y.contains("\"/scm/**/info/refs\"") && y.contains("\"/scm/**/git-upload-pack\""));
        assert!(y.contains("\"/scm/PROJ/app.git/git-receive-pack\""));
        assert!(y.contains("\"/scm/PROJ/app/git-receive-pack\""));
        assert!(y.contains("display_name: Bitbucket Data Center example.net/PROJ/app"));
        assert_safe(&y);
    }

    #[test]
    fn host_answers_round_trip_and_none_is_remembered() {
        let dir = std::env::temp_dir().join(format!("osh-hosts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("git-hosts");
        assert_eq!(host_entry(Some(&file), "a.test"), None);
        remember_host(Some(&file), "a.test", Some(Forge::GitLab), Some(2222));
        remember_host(Some(&file), "b.test", None, None);
        remember_host(
            Some(&file),
            "bitbucket.org",
            Some(Forge::BitbucketCloud),
            None,
        );
        remember_host(Some(&file), "bad host", Some(Forge::GitLab), None);
        let a = host_entry(Some(&file), "a.test").unwrap();
        assert_eq!((a.forge, a.ssh_port), (Some(Forge::GitLab), Some(2222)));
        assert_eq!(host_entry(Some(&file), "b.test").unwrap().forge, None);
        assert_eq!(
            host_entry(Some(&file), "bitbucket.org").unwrap().forge,
            Some(Forge::BitbucketCloud)
        );
        remember_host(Some(&file), "a.test", Some(Forge::Forgejo), None);
        let a = host_entry(Some(&file), "a.test").unwrap();
        assert_eq!((a.forge, a.ssh_port), (Some(Forge::Forgejo), None));
        assert_eq!(host_entry(Some(&file), "bad host"), None);
        assert_eq!(load_hosts(Some(&file)).len(), 3);
        assert_eq!(host_entry(None, "a.test"), None);
        assert!(hosts_path().is_none(), "no file under test");
        // An old "is it Forgejo?" yes still counts; a no is asked again; the new file wins.
        std::fs::write(
            dir.join("forgejo-hosts"),
            "old.test\tyes\nno.test\tno\na.test\tyes\n",
        )
        .unwrap();
        remember_host(Some(&file), "a.test", Some(Forge::GitLab), None);
        assert_eq!(
            host_entry(Some(&file), "old.test").unwrap().forge,
            Some(Forge::Forgejo)
        );
        assert_eq!(host_entry(Some(&file), "no.test"), None);
        assert_eq!(
            host_entry(Some(&file), "a.test").unwrap().forge,
            Some(Forge::GitLab)
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
