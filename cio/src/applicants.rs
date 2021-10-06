#![allow(clippy::from_over_into)]
use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    io::{copy, Write},
    process::Command,
    str::FromStr,
};

use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::{offset::Utc, DateTime, Duration, NaiveDate};
use chrono_humanize::HumanTime;
use docusign::DocuSign;
use flate2::read::GzDecoder;
use google_drive::{
    traits::{DriveOps, FileOps},
    Client as GoogleDrive,
};
use google_geocode::Geocode;
use html2text::from_read;
use log::{info, warn};
use macros::db;
use regex::Regex;
use schemars::JsonSchema;
use sendgrid_api::{traits::MailOps, Client as SendGrid};
use serde::{Deserialize, Serialize};
use sheets::Client as GoogleSheets;
use slack_chat_api::{
    FormattedMessage, MessageAttachment, MessageBlock, MessageBlockText, MessageBlockType, MessageType,
};
use tar::Archive;
use walkdir::WalkDir;

use crate::{
    airtable::{AIRTABLE_APPLICATIONS_TABLE, AIRTABLE_REVIEWER_LEADERBOARD_TABLE},
    applicant_reviews::ApplicantReview,
    companies::Company,
    configs::{User, Users},
    core::UpdateAirtableRecord,
    db::Database,
    interviews::ApplicantInterview,
    schema::{applicant_interviews, applicant_reviewers, applicants, users},
    utils::{check_if_github_issue_exists, get_value, truncate},
};

// The line breaks that get parsed are weird thats why we have the random asterisks here.
static QUESTION_TECHNICALLY_CHALLENGING: &str =
    r"W(?s:.*)at work(?s:.*)ave you found mos(?s:.*)challenging(?s:.*)caree(?s:.*)wh(?s:.*)\?";
static QUESTION_WORK_PROUD_OF: &str =
    r"W(?s:.*)at work(?s:.*)ave you done that you(?s:.*)particularl(?s:.*)proud o(?s:.*)and why\?";
static QUESTION_HAPPIEST_CAREER: &str =
    r"W(?s:.*)en have you been happiest in your professiona(?s:.*)caree(?s:.*)and why\?";
static QUESTION_UNHAPPIEST_CAREER: &str =
    r"W(?s:.*)en have you been unhappiest in your professiona(?s:.*)caree(?s:.*)and why\?";
static QUESTION_VALUE_REFLECTED: &str = r"F(?s:.*)r one of Oxide(?s:.*)s values(?s:.*)describe an example of ho(?s:.*)it wa(?s:.*)reflected(?s:.*)particula(?s:.*)body(?s:.*)you(?s:.*)work\.";
static QUESTION_VALUE_VIOLATED: &str = r"F(?s:.*)r one of Oxide(?s:.*)s values(?s:.*)describe an example of ho(?s:.*)it wa(?s:.*)violated(?s:.*)you(?s:.*)organization o(?s:.*)work\.";
static QUESTION_VALUES_IN_TENSION: &str = r"F(?s:.*)r a pair of Oxide(?s:.*)s values(?s:.*)describe a time in whic(?s:.*)the tw(?s:.*)values(?s:.*)tensio(?s:.*)for(?s:.*)your(?s:.*)and how yo(?s:.*)resolved it\.";
static QUESTION_WHY_OXIDE: &str =
    r"W(?s:.*)y(?s:.*)do(?s:.*)you(?s:.*)want(?s:.*)to(?s:.*)work(?s:.*)for(?s:.*)Oxide\?";

pub static DOCUSIGN_OFFER_TEMPLATE: &str = "Employee Offer Letter (US)";
pub static DOCUSIGN_OFFER_SUBJECT: &str = "Sign your Oxide Computer Company Offer Letter";
pub static DOCUSIGN_PIIA_TEMPLATE: &str = "Employee Agreements (Mediation, PIIA)";
pub static DOCUSIGN_PIIA_SUBJECT: &str = "Sign your Oxide Computer Company Employee Agreements";

/// The data type for a NewApplicant.
#[db {
    new_struct_name = "Applicant",
    airtable_base = "hiring",
    airtable_table = "AIRTABLE_APPLICATIONS_TABLE",
    match_on = {
        "email" = "String",
        "sheet_id" = "String",
    },
}]
#[derive(Debug, Insertable, AsChangeset, PartialEq, Clone, JsonSchema, Deserialize, Serialize)]
#[table_name = "applicants"]
pub struct NewApplicant {
    pub name: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sheet_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw_status: String,
    pub submitted_time: DateTime<Utc>,
    pub email: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub phone: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub country_code: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub location: String,
    #[serde(default)]
    pub latitude: f32,
    #[serde(default)]
    pub longitude: f32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub github: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gitlab: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub linkedin: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub portfolio: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub portfolio_pdf: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub website: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resume: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub materials: String,
    #[serde(default)]
    pub sent_email_received: bool,
    #[serde(default)]
    pub sent_email_follow_up: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_sent_date_time: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value_reflected: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub value_violated: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values_in_tension: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resume_contents: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub materials_contents: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub work_samples: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub writing_samples: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub analysis_samples: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub presentation_samples: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub exploratory_samples: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question_technically_challenging: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question_proud_of: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question_happiest: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question_unhappiest: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question_value_reflected: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question_value_violated: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question_values_in_tension: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question_why_oxide: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub interview_packet: String,
    /// Airtable fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interviews: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interviews_started: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interviews_completed: Option<DateTime<Utc>>,

    /// The scorers/reviewers assigned to the applicant.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "airtable_api::user_format_as_array_of_strings::serialize",
        deserialize_with = "airtable_api::user_format_as_array_of_strings::deserialize"
    )]
    pub scorers: Vec<String>,
    /// The scorers_completed field means the person has already reviewed the applicant.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        serialize_with = "airtable_api::user_format_as_array_of_strings::serialize",
        deserialize_with = "airtable_api::user_format_as_array_of_strings::deserialize"
    )]
    pub scorers_completed: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scoring_form_id: String,
    /// The form for scoring/evaluating applicants.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scoring_form_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scoring_form_responses_url: String,
    /// The number of form responses for the applicant.
    #[serde(default)]
    pub scoring_evaluations_count: i32,
    #[serde(default)]
    pub scoring_enthusiastic_yes_count: i32,
    #[serde(default)]
    pub scoring_yes_count: i32,
    #[serde(default)]
    pub scoring_pass_count: i32,
    #[serde(default)]
    pub scoring_no_count: i32,
    #[serde(default)]
    pub scoring_not_applicable_count: i32,
    #[serde(default)]
    pub scoring_insufficient_experience_count: i32,
    #[serde(default)]
    pub scoring_inapplicable_experience_count: i32,
    #[serde(default)]
    pub scoring_job_function_yet_needed_count: i32,
    #[serde(default)]
    pub scoring_underwhelming_materials_count: i32,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub criminal_background_check_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub motor_vehicle_background_check_status: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_date: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interested_in: Vec<String>,

    /// This field is used by Airtable for mapping the location data.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub geocode_cache: String,

    /// These fields are used by the DocuSign integration.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub docusign_envelope_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub docusign_envelope_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer_created: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer_completed: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub docusign_piia_envelope_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub docusign_piia_envelope_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub piia_envelope_created: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub piia_envelope_completed: Option<DateTime<Utc>>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub link_to_reviews: Vec<String>,

    /// The CIO company ID.
    #[serde(default)]
    pub cio_company_id: i32,
}

pub fn clean_interested_in(st: &str) -> String {
    let s = st.trim().to_lowercase();

    if s == "product security engineer" || s == "security engineer" || s == "software engineer - security" {
        return "Product Security Engineer".to_string();
    } else if s == "software engineer: web" {
        return "Software Engineer: Web".to_string();
    } else if s == "software engineer: embedded systems" {
        return "Software Engineer: Embedded Systems".to_string();
    } else if s == "software engineer: control plane" {
        return "Software Engineer: Control Plane".to_string();
    } else if s == "hardware engineer" {
        return "Hardware Engineer".to_string();
    }

    st.to_string()
}

impl NewApplicant {
    pub async fn send_slack_notification(&self, db: &Database, company: &Company) -> Result<()> {
        let mut msg: FormattedMessage = self.clone().into();
        // Set the channel.
        msg.channel = company.slack_channel_applicants.to_string();
        // Post the message.
        company.post_to_slack_channel(db, &msg).await?;

        Ok(())
    }

    /// Parse the sheet columns from single Google Sheets row values.
    /// This is what we get back from the webhook.
    pub async fn parse_from_row(sheet_id: &str, values: &HashMap<String, Vec<String>>) -> Self {
        // Fill in the data we know from what we got from the row.
        let (github, gitlab) = NewApplicant::parse_github_gitlab(&get_value(values, "GitHub Profile URL"));

        let interested_in_string = get_value(values, "Which job descriptions are you interested in?");
        let split = interested_in_string.trim().split(',');
        let interested_in_str: Vec<&str> = split.collect();
        let mut interested_in: Vec<String> = Default::default();
        for s in interested_in_str {
            let i = clean_interested_in(s);
            if !i.is_empty() {
                interested_in.push(i.to_string());
            }
        }

        let location = get_value(values, "Location (City, State or Region)");
        // Create the geocode client.
        let geocode = Geocode::new_from_env();
        let mut latitude = 0.0;
        let mut longitude = 0.0;
        // Attempt to get the lat and lng.
        match geocode.get(&location).await {
            Ok(result) => {
                let location = result.geometry.location;
                latitude = location.lat as f32;
                longitude = location.lng as f32;
            }
            Err(e) => {
                if !location.is_empty() {
                    warn!("could not get lat lng for location `{}`: {}", location, e);
                }
            }
        }

        NewApplicant {
            submitted_time: NewApplicant::parse_timestamp(&get_value(values, "Timestamp")),
            role: get_role_from_sheet_id(sheet_id),
            sheet_id: sheet_id.to_string(),
            name: get_value(values, "Name"),
            email: get_value(values, "Email Address"),
            location,
            latitude,
            longitude,
            phone: get_value(values, "Phone Number"),
            country_code: Default::default(),
            github,
            gitlab,
            linkedin: get_value(values, "LinkedIn profile URL"),
            portfolio: get_value(values, "Portfolio"),
            portfolio_pdf: Default::default(),
            website: get_value(values, "Website"),
            resume: get_value(values, "Submit your resume (or PDF export of LinkedIn profile)"),
            materials: get_value(values, "Submit your Oxide candidate materials"),
            status: crate::applicant_status::Status::NeedsToBeTriaged.to_string(),
            raw_status: get_value(values, "Status"),
            sent_email_received: false,
            sent_email_follow_up: false,
            rejection_sent_date_time: None,
            value_reflected: Default::default(),
            value_violated: Default::default(),
            values_in_tension: Default::default(),
            resume_contents: Default::default(),
            materials_contents: Default::default(),
            work_samples: Default::default(),
            writing_samples: Default::default(),
            analysis_samples: Default::default(),
            presentation_samples: Default::default(),
            exploratory_samples: Default::default(),
            question_technically_challenging: Default::default(),
            question_proud_of: Default::default(),
            question_happiest: Default::default(),
            question_unhappiest: Default::default(),
            question_value_reflected: Default::default(),
            question_value_violated: Default::default(),
            question_values_in_tension: Default::default(),
            question_why_oxide: Default::default(),
            interview_packet: Default::default(),
            interviews: Default::default(),
            interviews_started: Default::default(),
            interviews_completed: Default::default(),
            scorers: Default::default(),
            scorers_completed: Default::default(),
            scoring_form_id: Default::default(),
            scoring_form_url: Default::default(),
            scoring_form_responses_url: Default::default(),
            scoring_evaluations_count: Default::default(),
            scoring_enthusiastic_yes_count: Default::default(),
            scoring_yes_count: Default::default(),
            scoring_pass_count: Default::default(),
            scoring_no_count: Default::default(),
            scoring_not_applicable_count: Default::default(),
            scoring_insufficient_experience_count: Default::default(),
            scoring_inapplicable_experience_count: Default::default(),
            scoring_job_function_yet_needed_count: Default::default(),
            scoring_underwhelming_materials_count: Default::default(),
            criminal_background_check_status: Default::default(),
            motor_vehicle_background_check_status: Default::default(),
            start_date: None,
            interested_in,
            geocode_cache: Default::default(),
            docusign_envelope_id: Default::default(),
            docusign_envelope_status: Default::default(),
            offer_created: Default::default(),
            offer_completed: Default::default(),
            docusign_piia_envelope_id: Default::default(),
            docusign_piia_envelope_status: Default::default(),
            piia_envelope_created: Default::default(),
            piia_envelope_completed: Default::default(),
            link_to_reviews: Default::default(),
            // TODO: update this, when we support multiple companies.
            cio_company_id: 1,
        }
    }

    /// Send an email to the applicant that we recieved their application.
    pub async fn send_email_recieved_application_to_applicant(&self, db: &Database) -> Result<()> {
        let company = self.company(db)?;
        // Initialize the SendGrid client.
        let sendgrid_client = SendGrid::new_from_env();

        // Send the message.
        sendgrid_client
            .mail_send()
            .send_plain_text(
                &format!(
                    "Oxide Computer Company {} Application Received for {}",
                    self.role, self.name
                ),
                &format!(
                    "Dear {},

Thank you for submitting your application materials! We really appreciate all
the time and thought everyone puts into their application. We will be in touch
within the next few weeks with more information. Just a heads up this could take
up to 4-6 weeks.

Sincerely,
  The Oxide Team",
                    self.name
                ),
                &[self.email.to_string()],
                &[format!("careers@{}", company.gsuite_domain)],
                &[],
                &format!("careers@{}", company.gsuite_domain),
            )
            .await?;

        Ok(())
    }

    /// Send an email to the applicant that we love them but they are too junior.
    pub async fn send_email_rejection_junior_but_we_love_you(&self, db: &Database) -> Result<()> {
        let company = self.company(db)?;
        // Initialize the SendGrid client.
        let sendgrid_client = SendGrid::new_from_env();

        // Send the message.
        sendgrid_client
            .mail_send()
            .send_plain_text(
                &format!("Thank you for your application, {}", self.name),
                &format!(
                    "Dear {},

Thank you for your application to join Oxide Computer Company. At this point
in time, we are focusing on hiring engineers with professional experience,
who have a track record of self-directed contributions to a team.

We are grateful you took the time to apply and put so much thought into
your candidate materials, we loved reading them. Although engineers at the
early stages of their career are unlikely to be a fit for us right now, we
are growing, and encourage you to consider re-applying in the future.

 We would absolutely love to work with you in the future and cannot wait for
that stage of the company!

All the best,
The Oxide Team",
                    self.name
                ),
                &[self.email.to_string()],
                &[format!("careers@{}", company.gsuite_domain)],
                &[],
                &format!("careers@{}", company.gsuite_domain),
            )
            .await?;

        Ok(())
    }

    /// Send an email to the applicant that they did not provide materials.
    pub async fn send_email_rejection_did_not_provide_materials(&self, db: &Database) -> Result<()> {
        let company = self.company(db)?;
        // Initialize the SendGrid client.
        let sendgrid_client = SendGrid::new_from_env();

        // Send the message.
        sendgrid_client
            .mail_send()
            .send_plain_text(
                &format!("Thank you for your application, {}", self.name),
                &format!(
                    "Dear {},

Unfortunately, we cannot accept it at this time since you failed to provide the
requested materials.

All the best,
The Oxide Team",
                    self.name
                ),
                &[self.email.to_string()],
                &[format!("careers@{}", company.gsuite_domain)],
                &[],
                &format!("careers@{}", company.gsuite_domain),
            )
            .await?;

        Ok(())
    }

    /// Send an email to the applicant about timing.
    pub async fn send_email_rejection_timing(&self, db: &Database) -> Result<()> {
        let company = self.company(db)?;
        // Initialize the SendGrid client.
        let sendgrid_client = SendGrid::new_from_env();

        // Send the message.
        sendgrid_client
            .mail_send()
            .send_plain_text(
                &format!("Thank you for your application, {}", self.name),
                &format!(
                    "Dear {},

We are so humbled by your application to join Oxide Computer Company. At this
stage of the company we are hyper-focused on certain areas of the stack and
when we need specific domain space experience such as yours, please engage
with us. Our roles will be updated as we need them.

We are grateful you took the time to apply and put so much thought into the
candidate materials, we loved reading them. We would absolutely love to work
with you in the future and cannot wait for that stage of the company!

All the best,
The Oxide Team",
                    self.name
                ),
                &[self.email.to_string()],
                &[format!("careers@{}", company.gsuite_domain)],
                &[],
                &format!("careers@{}", company.gsuite_domain),
            )
            .await?;

        Ok(())
    }

    /// Send an email internally that we have a new application.
    pub async fn send_email_internally(&self, db: &Database) -> Result<()> {
        let company = self.company(db)?;
        // Initialize the SendGrid client.
        let sendgrid_client = SendGrid::new_from_env();

        // Send the message.
        sendgrid_client
            .mail_send()
            .send_plain_text(
                &format!("New {} Application: {}", self.role, self.name),
                &self.as_company_notification_email(),
                &[format!("applications@{}", company.gsuite_domain)],
                &[],
                &[],
                &format!("applications@{}", company.gsuite_domain),
            )
            .await?;

        Ok(())
    }

    /// Parse the applicant from a Google Sheets row, where we also happen to know the columns.
    /// This is how we get the spreadsheet back from the API.
    pub async fn parse_from_row_with_columns(
        sheet_name: &str,
        sheet_id: &str,
        columns: &ApplicantSheetColumns,
        row: &[String],
    ) -> Self {
        // If the length of the row is greater than the status column
        // then we have a status.
        let raw_status = if row.len() > columns.status {
            row[columns.status].to_string()
        } else {
            "".to_string()
        };
        let mut status = crate::applicant_status::Status::from_str(&raw_status).unwrap_or_default();

        let (github, gitlab) = NewApplicant::parse_github_gitlab(&row[columns.github]);

        // If the length of the row is greater than the linkedin column
        // then we have a linkedin.
        let mut linkedin = if row.len() > columns.linkedin && columns.linkedin != 0 {
            row[columns.linkedin].trim().to_lowercase()
        } else {
            "".to_string()
        };
        if linkedin == "n/a" {
            linkedin = "".to_string();
        }
        if !linkedin.is_empty() {
            // Cleanup linkedin link.
            linkedin = format!(
                "https://linkedin.com/{}",
                linkedin
                    .trim_start_matches("https://linkedin.com/")
                    .trim_start_matches("https://www.linkedin.com/")
                    .trim_start_matches("http://linkedin.com/")
                    .trim_start_matches("http://www.linkedin.com/")
                    .trim_start_matches("www.linkedin.com/")
                    .trim_start_matches("linkedin.com/")
                    .trim()
            );
        }

        // If the length of the row is greater than the start date column
        // then we have a start date.
        let mut start_date = if row.len() > columns.start_date && columns.start_date != 0 {
            if row[columns.start_date].trim().is_empty() {
                None
            } else {
                Some(NaiveDate::parse_from_str(row[columns.start_date].trim(), "%m/%d/%Y").unwrap())
            }
        } else {
            None
        };

        // If the length of the row is greater than the interested in column
        // then we have an interest.
        let interested_in_str: Vec<&str> = if row.len() > columns.interested_in && columns.interested_in != 0 {
            if row[columns.interested_in].trim().is_empty() {
                vec![]
            } else {
                let split = row[columns.interested_in].trim().split(',');
                split.collect()
            }
        } else {
            vec![]
        };
        let mut interested_in: Vec<String> = Default::default();
        for s in interested_in_str {
            let i = clean_interested_in(s);
            if !i.is_empty() {
                interested_in.push(i.to_string());
            }
        }

        // If the length of the row is greater than the portfolio column
        // then we have a portfolio.
        let portfolio = if row.len() > columns.portfolio && columns.portfolio != 0 {
            row[columns.portfolio].trim().to_string()
        } else {
            "".to_lowercase()
        };

        // If the length of the row is greater than the website column
        // then we have a website.
        let website = if row.len() > columns.website && columns.website != 0 {
            row[columns.website].trim().to_lowercase()
        } else {
            "".to_lowercase()
        };

        // If the length of the row is greater than the value_reflected column
        // then we have a value_reflected.
        let mut value_reflected = if row.len() > columns.value_reflected && columns.value_reflected != 0 {
            row[columns.value_reflected].trim().to_lowercase()
        } else {
            "".to_lowercase()
        };

        // If the length of the row is greater than the value_violated column
        // then we have a value_violated.
        let mut value_violated = if row.len() > columns.value_violated && columns.value_violated != 0 {
            row[columns.value_violated].trim().to_lowercase()
        } else {
            "".to_lowercase()
        };

        let mut values_in_tension: Vec<String> = Default::default();
        // If the length of the row is greater than the value_in_tension1 column
        // then we have a value_in_tension1.
        if row.len() > columns.value_in_tension_1 && columns.value_in_tension_1 != 0 {
            let value_1 = row[columns.value_in_tension_1].trim().to_lowercase();
            if !value_1.is_empty() {
                values_in_tension.push(value_1);
            }
        }
        // If the length of the row is greater than the value_in_tension2 column
        // then we have a value_in_tension2.
        if row.len() > columns.value_in_tension_2 && columns.value_in_tension_2 != 0 {
            let value_2 = row[columns.value_in_tension_2].trim().to_lowercase();
            if !value_2.is_empty() {
                values_in_tension.push(value_2);
            }
        }
        values_in_tension.sort();

        // Check if we sent them an email that we received their application.
        let mut sent_email_received = true;
        if row[columns.sent_email_received].to_lowercase().contains("false") {
            sent_email_received = false;
        }

        // Check if we sent them an email to either reject or follow up.
        let mut sent_email_follow_up = true;
        if row[columns.sent_email_follow_up].to_lowercase().contains("false") {
            sent_email_follow_up = false;
        }

        let mut rejection_sent_date_time = None;

        let email = row[columns.email].trim().to_string();
        let location = row[columns.location].trim().to_string();
        let mut latitude = 0.0;
        let mut longitude = 0.0;
        let phone = row[columns.phone].trim().to_string();
        let mut country_code = "".to_string();
        let resume = row[columns.resume].to_string();
        let materials = row[columns.materials].to_string();

        let mut resume_contents = String::new();
        let mut materials_contents = String::new();
        let mut work_samples = String::new();
        let mut writing_samples = String::new();
        let mut analysis_samples = String::new();
        let mut presentation_samples = String::new();
        let mut exploratory_samples = String::new();
        let mut question_technically_challenging = String::new();
        let mut question_proud_of = String::new();
        let mut question_happiest = String::new();
        let mut question_unhappiest = String::new();
        let mut question_value_reflected = String::new();
        let mut question_value_violated = String::new();
        let mut question_values_in_tension = String::new();
        let mut question_why_oxide = String::new();

        let mut interviews: Vec<String> = Default::default();
        let mut interviews_started = Default::default();
        let mut interviews_completed = Default::default();
        let mut interview_packet = String::new();

        let mut scorers: Vec<String> = Default::default();
        let mut scorers_completed: Vec<String> = Default::default();
        let mut scoring_form_id = "".to_string();
        let mut scoring_form_url = "".to_string();
        let mut scoring_form_responses_url = "".to_string();

        // Set the defaults.
        let mut scoring_evaluations_count = 0;
        let mut scoring_enthusiastic_yes_count = 0;
        let mut scoring_yes_count = 0;
        let mut scoring_pass_count = 0;
        let mut scoring_no_count = 0;
        let mut scoring_not_applicable_count = 0;
        let mut scoring_insufficient_experience_count = 0;
        let mut scoring_inapplicable_experience_count = 0;
        let mut scoring_job_function_yet_needed_count = 0;
        let mut scoring_underwhelming_materials_count = 0;

        let mut criminal_background_check_status = "".to_string();
        let mut motor_vehicle_background_check_status = "".to_string();

        let mut docusign_envelope_id = "".to_string();
        let mut docusign_envelope_status = "".to_string();

        let mut offer_created = None;
        let mut offer_completed = None;

        let mut docusign_piia_envelope_id = "".to_string();
        let mut docusign_piia_envelope_status = "".to_string();

        let mut piia_envelope_created = None;
        let mut piia_envelope_completed = None;

        let mut airtable_record_id = "".to_string();

        // TODO: do not hard code the company id.
        let mut cio_company_id = 1;

        // Try to get the applicant, if they exist.
        // This is a way around the stupid magic macro to make sure it
        // doesn't overwrite fields set by other functions on the upsert.
        // TODO: this is gross and disgusting.
        let db = Database::new();
        if let Ok(a) = applicants::dsl::applicants
            .filter(applicants::dsl::email.eq(email.to_string()))
            .filter(applicants::dsl::sheet_id.eq(sheet_id.to_string()))
            .first::<Applicant>(&db.conn())
        {
            // Try to get from airtable.
            // This ensures if we had any one offs added in airtable that they stay intact.
            if let Some(record) = a.get_existing_airtable_record(&db).await {
                scorers = record.fields.scorers;
                scorers_completed = a.scorers_completed;
                interviews = record.fields.interviews;
            }

            airtable_record_id = a.airtable_record_id.to_string();

            if a.interviews_started.is_some() {
                interviews_started = a.interviews_started;
            }
            if a.interviews_completed.is_some() {
                interviews_completed = a.interviews_completed;
            }

            if a.rejection_sent_date_time.is_some() {
                rejection_sent_date_time = a.rejection_sent_date_time;
            }

            // If the database has them as "Onboarding" and we have them as "Giving offer",
            // then use what is in the database.
            // This status change happens when the docusign offer is signed (so it is not
            // propogated back to the spreadsheet).
            // Therefore, the spreadsheet cannot be used as the source of truth.
            if a.status == crate::applicant_status::Status::Onboarding.to_string()
                && status == crate::applicant_status::Status::GivingOffer
            {
                status = crate::applicant_status::Status::Onboarding;
            }

            if !a.docusign_envelope_id.is_empty() {
                docusign_envelope_id = a.docusign_envelope_id.to_string();
            }
            if !a.docusign_envelope_status.is_empty() {
                docusign_envelope_status = a.docusign_envelope_status.to_string();
            }
            if a.offer_created.is_some() {
                offer_created = a.offer_created;
            }
            if a.offer_completed.is_some() {
                offer_completed = a.offer_completed;
            }

            if !a.docusign_piia_envelope_id.is_empty() {
                docusign_piia_envelope_id = a.docusign_piia_envelope_id.to_string();
            }
            if !a.docusign_piia_envelope_status.is_empty() {
                docusign_piia_envelope_status = a.docusign_piia_envelope_status.to_string();
            }
            if a.piia_envelope_created.is_some() {
                piia_envelope_created = a.piia_envelope_created;
            }
            if a.piia_envelope_completed.is_some() {
                piia_envelope_completed = a.piia_envelope_completed;
            }

            if !a.country_code.is_empty() {
                country_code = a.country_code.to_string();
            }
            latitude = a.latitude;
            longitude = a.longitude;

            if !a.interview_packet.is_empty() {
                interview_packet = a.interview_packet.to_string();
            }

            resume_contents = a.resume_contents.to_string();
            materials_contents = a.materials_contents.to_string();
            work_samples = a.work_samples.to_string();
            writing_samples = a.writing_samples.to_string();
            analysis_samples = a.analysis_samples.to_string();
            presentation_samples = a.presentation_samples.to_string();
            exploratory_samples = a.exploratory_samples.to_string();
            question_technically_challenging = a.question_technically_challenging.to_string();
            question_proud_of = a.question_proud_of.to_string();
            question_happiest = a.question_happiest.to_string();
            question_unhappiest = a.question_unhappiest.to_string();
            question_value_reflected = a.question_value_reflected.to_string();
            question_value_violated = a.question_value_violated.to_string();
            question_values_in_tension = a.question_values_in_tension.to_string();
            question_why_oxide = a.question_why_oxide.to_string();

            if !a.value_reflected.is_empty() {
                value_reflected = a.value_reflected.to_string();
            }
            if !a.value_violated.is_empty() {
                value_violated = a.value_violated.to_string();
            }
            if !a.values_in_tension.is_empty() {
                values_in_tension = a.values_in_tension.clone();
                values_in_tension.sort();
            }
            // Set the scorers data so we don't keep adding new scorers.
            if !a.scorers.is_empty() {
                scoring_form_id = a.scoring_form_id.to_string();
                scoring_form_url = a.scoring_form_url.to_string();
                scoring_form_responses_url = a.scoring_form_responses_url.to_string();

                scoring_evaluations_count = a.scoring_evaluations_count;
                scoring_enthusiastic_yes_count = a.scoring_enthusiastic_yes_count;
                scoring_yes_count = a.scoring_yes_count;
                scoring_pass_count = a.scoring_pass_count;
                scoring_no_count = a.scoring_no_count;
                scoring_not_applicable_count = a.scoring_not_applicable_count;
                scoring_insufficient_experience_count = a.scoring_insufficient_experience_count;
                scoring_inapplicable_experience_count = a.scoring_inapplicable_experience_count;
                scoring_job_function_yet_needed_count = a.scoring_job_function_yet_needed_count;
                scoring_underwhelming_materials_count = a.scoring_underwhelming_materials_count;
            }
            if !a.criminal_background_check_status.is_empty() {
                criminal_background_check_status = a.criminal_background_check_status.to_string();
            }
            if !a.motor_vehicle_background_check_status.is_empty() {
                motor_vehicle_background_check_status = a.criminal_background_check_status.to_string();
            }

            // The start date might be set by docusign, in that case we want it to propgate.
            if start_date.is_none() && a.start_date.is_some() {
                start_date = a.start_date;
            }

            cio_company_id = a.cio_company_id;
        }

        // If we know they have more than 1 interview AND their current status is "next steps",
        // THEN we can mark the applicant as in the "interviewing" state.
        if interviews.len() > 1
            && (status == crate::applicant_status::Status::NextSteps
                || status == crate::applicant_status::Status::NeedsToBeTriaged)
        {
            status = crate::applicant_status::Status::Interviewing;
        }
        // If their status is "Onboarding" and it is after their start date.
        // Set their status to "Hired".
        if (status == crate::applicant_status::Status::Onboarding
            || status == crate::applicant_status::Status::GivingOffer)
            && start_date.is_some()
            && start_date.unwrap() <= Utc::now().date().naive_utc()
        {
            // We shouldn't also check if we have an employee for the user, only if the employee had
            // been hired and left.
            status = crate::applicant_status::Status::Hired;
        }

        // Get the latitude and longitude if we don't already have it.
        if latitude == 0.0 && longitude == 0.0 {
            // Create the geocode client.
            let geocode = Geocode::new_from_env();
            // Attempt to get the lat and lng.
            match geocode.get(&location).await {
                Ok(result) => {
                    let location = result.geometry.location;
                    latitude = location.lat as f32;
                    longitude = location.lng as f32;
                }
                Err(e) => {
                    if !location.is_empty() {
                        warn!("could not get lat lng for location `{}`: {}", location, e);
                    }
                }
            }
        }

        // If we have interviews for them, let's update the interviews_started and
        // interviews_completed times.
        if !interviews.is_empty() && !airtable_record_id.is_empty() {
            // Since our length is at least one, we must have at least one interview.
            // Let's query the interviews for this candidate.
            let data = applicant_interviews::dsl::applicant_interviews
                .filter(applicant_interviews::dsl::applicant.contains(vec![airtable_record_id.to_string()]))
                .order_by(applicant_interviews::dsl::start_time.asc())
                .load::<ApplicantInterview>(&db.conn())
                .unwrap();
            for (index, r) in data.iter().enumerate() {
                if index == 0 {
                    // We have the first record.
                    // Let's update the started time.
                    interviews_started = Some(r.start_time);
                    // We continue here so we don't accidentally set the
                    // completed_time if we only have one record.
                    continue;
                }
                if index == data.len() - 1 {
                    // We are on the last record.
                    // Let's update the completed time.
                    interviews_completed = Some(r.end_time);
                    break;
                }
            }
        }

        NewApplicant {
            submitted_time: NewApplicant::parse_timestamp(&row[columns.timestamp]),
            name: row[columns.name].to_string(),
            email,
            location,
            latitude,
            longitude,
            phone,
            country_code,
            github,
            gitlab,
            linkedin,
            portfolio,
            portfolio_pdf: Default::default(),
            website,
            resume,
            materials,
            status: status.to_string(),
            raw_status,
            sent_email_received,
            sent_email_follow_up,
            rejection_sent_date_time,
            role: sheet_name.to_string(),
            sheet_id: sheet_id.to_string(),
            value_reflected,
            value_violated,
            values_in_tension,
            resume_contents,
            materials_contents,
            work_samples,
            writing_samples,
            analysis_samples,
            presentation_samples,
            exploratory_samples,
            question_technically_challenging,
            question_proud_of,
            question_happiest,
            question_unhappiest,
            question_value_reflected,
            question_value_violated,
            question_values_in_tension,
            question_why_oxide,
            interview_packet,
            interviews,
            interviews_started,
            interviews_completed,
            scorers,
            scorers_completed,
            scoring_form_id,
            scoring_form_url,
            scoring_form_responses_url,
            scoring_evaluations_count,
            scoring_enthusiastic_yes_count,
            scoring_yes_count,
            scoring_pass_count,
            scoring_no_count,
            scoring_not_applicable_count,
            scoring_insufficient_experience_count,
            scoring_inapplicable_experience_count,
            scoring_job_function_yet_needed_count,
            scoring_underwhelming_materials_count,
            criminal_background_check_status,
            motor_vehicle_background_check_status,
            start_date,
            interested_in,
            geocode_cache: Default::default(),
            docusign_envelope_id,
            docusign_envelope_status,
            offer_created,
            offer_completed,
            docusign_piia_envelope_id,
            docusign_piia_envelope_status,
            piia_envelope_created,
            piia_envelope_completed,
            link_to_reviews: Default::default(),
            cio_company_id,
        }
    }

    fn parse_timestamp(timestamp: &str) -> DateTime<Utc> {
        // Parse the time.
        let time_str = timestamp.to_owned() + " -08:00";
        DateTime::parse_from_str(&time_str, "%m/%d/%Y %H:%M:%S  %:z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn parse_github_gitlab(s: &str) -> (String, String) {
        let mut github = "".to_string();
        let mut gitlab = "".to_string();
        if !s.trim().is_empty() {
            github = format!(
                "@{}",
                s.trim()
                    .to_lowercase()
                    .trim_start_matches("https://github.com/")
                    .trim_start_matches("http://github.com/")
                    .trim_start_matches("https://www.github.com/")
                    .trim_start_matches("http://www.github.com/")
                    .trim_start_matches("www.github.com/")
                    .trim_start_matches("github.com/")
                    .trim_start_matches('@')
                    .replace("github.com", "")
                    .trim_end_matches('/')
                    .trim_start_matches('/')
            )
            .trim()
            .to_string();

            if github == "@" || github == "@n/a" || github.contains("linkedin.com") {
                github = "".to_string();
            }

            // Some people put a gitlab URL in the github form input,
            // parse those accordingly.
            if github.contains("https://gitlab.com") {
                github = "".to_string();

                gitlab = format!(
                    "@{}",
                    s.trim()
                        .to_lowercase()
                        .trim_start_matches("https://gitlab.com/")
                        .trim_start_matches('@')
                        .trim_end_matches('/')
                );
            }
        }

        (github, gitlab)
    }

    /// Expand the applicants materials and do any automation that needs to be done.
    pub async fn expand(
        &mut self,
        db: &Database,
        drive_client: &GoogleDrive,
        sheets_client: &GoogleSheets,
        sent_email_received_column_index: usize,
        sent_email_follow_up_index: usize,
        row_index: usize,
    ) -> Result<()> {
        // Check if we have sent them an email that we received their application.
        if !self.sent_email_received {
            // Send them an email.
            self.send_email_recieved_application_to_applicant(db).await?;

            // Mark the column as true not false.
            let mut colmn = "ABCDEFGHIJKLMNOPQRSTUVWXYZ".chars();
            let rng = format!(
                "{}{}",
                colmn.nth(sent_email_received_column_index).unwrap().to_string(),
                row_index
            );

            sheets_client
                .spreadsheets()
                .values_update(
                    &self.sheet_id,
                    &rng,
                    false, // include_values_in_response
                    sheets::types::DateTimeRenderOption::FormattedString,
                    sheets::types::ValueRenderOption::FormattedValue,
                    sheets::types::ValueInputOption::UserEntered,
                    &sheets::types::ValueRange {
                        range: rng.to_string(),
                        values: vec![vec!["TRUE".to_string()]],
                        major_dimension: None,
                    },
                )
                .await?;

            info!("sent email to {} that we received their application", self.email);
        }

        // Send an email follow up if we should.
        if !self.sent_email_follow_up {
            // Get the right cell to eventually change in the google sheet.
            let mut colmn = "ABCDEFGHIJKLMNOPQRSTUVWXYZ".chars();
            let rng = format!(
                "{}{}",
                colmn.nth(sent_email_follow_up_index).unwrap().to_string(),
                row_index
            );

            let status = crate::applicant_status::Status::from_str(&self.status).unwrap_or_default();
            if status == crate::applicant_status::Status::Declined
                || status == crate::applicant_status::Status::Deferred
            {
                // Check if we have sent the follow up email to them.unwrap_or_default().
                if self.raw_status.contains("did not do materials") {
                    // Send the email.
                    self.send_email_rejection_did_not_provide_materials(db).await?;

                    info!("sent email to {} tell them they did not do the materials", self.email);
                } else if self.raw_status.contains("junior") {
                    // Send the email.
                    self.send_email_rejection_junior_but_we_love_you(db).await?;

                    info!(
                        "sent email to {} tell them we can't hire them at this stage",
                        self.email
                    );
                } else {
                    // Send the email.
                    self.send_email_rejection_timing(db).await?;

                    info!("sent email to {} tell them about timing", self.email);
                }

                // Update the cell in the google sheet so we know we sent the email.
                // Mark the column as true not false.
                sheets_client
                    .spreadsheets()
                    .values_update(
                        &self.sheet_id,
                        &rng,
                        false, // include_values_in_response
                        sheets::types::DateTimeRenderOption::FormattedString,
                        sheets::types::ValueRenderOption::FormattedValue,
                        sheets::types::ValueInputOption::UserEntered,
                        &sheets::types::ValueRange {
                            range: rng.to_string(),
                            values: vec![vec!["TRUE".to_string()]],
                            major_dimension: None,
                        },
                    )
                    .await?;

                // Mark the time we sent the email.
                self.rejection_sent_date_time = Some(Utc::now());

                self.sent_email_follow_up = true;
            } else if status != crate::applicant_status::Status::NeedsToBeTriaged {
                // Just set that we have sent the email so that we don't do it again if we move to
                // next steps then interviews etc.
                // Only when it's not in "NeedsToBeTriaged".
                // Update the cell in the google sheet so we know we sent the email.
                // Mark the column as true not false.
                sheets_client
                    .spreadsheets()
                    .values_update(
                        &self.sheet_id,
                        &rng,
                        false, // include_values_in_response
                        sheets::types::DateTimeRenderOption::FormattedString,
                        sheets::types::ValueRenderOption::FormattedValue,
                        sheets::types::ValueInputOption::UserEntered,
                        &sheets::types::ValueRange {
                            range: rng.to_string(),
                            values: vec![vec!["TRUE".to_string()]],
                            major_dimension: None,
                        },
                    )
                    .await?;

                self.sent_email_follow_up = true;
            }
        }

        self.cleanup_phone();

        // Get the time seven days ago.
        let duration_from_now = Utc::now().signed_duration_since(self.submitted_time);

        // If the application is as new as the last week then parse all the contents.
        // This takes a long time so we skip all the others.
        if (duration_from_now < Duration::days(2)
            || (duration_from_now < Duration::days(20) && self.question_why_oxide.is_empty()))
            && self.status != crate::applicant_status::Status::Declined.to_string()
        {
            // Read the file contents.
            self.resume_contents = get_file_contents(drive_client, &self.resume).await?;
            self.materials_contents = get_file_contents(drive_client, &self.materials).await?;
            self.parse_materials();
        }

        Ok(())
    }

    /// Parse the questions from the materials.
    fn parse_materials(&mut self) {
        // Parse the samples and materials.
        let materials_contents = self.materials_contents.clone();
        let mut work_samples = parse_question(r"Work sample\(s\)", "Writing samples", &materials_contents);
        if work_samples.is_empty() {
            work_samples = parse_question(
                r"If(?s:.*)his work is entirely proprietary(?s:.*)please describe it as fully as y(?s:.*)can, providing necessary context\.",
                "Writing samples",
                &materials_contents,
            );
            if work_samples.is_empty() {
                // Try to parse work samples for TPM role.
                work_samples = parse_question(
                    r"What would you have done differently\?",
                    "Exploratory samples",
                    &materials_contents,
                );

                if work_samples.is_empty() {
                    work_samples = parse_question(
                        r"Some questions(?s:.*)o have in mind as you describe them:",
                        "Exploratory samples",
                        &materials_contents,
                    );

                    if work_samples.is_empty() {
                        work_samples = parse_question(r"Work samples", "Exploratory samples", &materials_contents);

                        if work_samples.is_empty() {
                            work_samples = parse_question(r"design sample\(s\)", "Questionnaire", &materials_contents);
                        }
                    }
                }
            }
        }
        self.work_samples = work_samples;

        let mut writing_samples = parse_question(r"Writing sample\(s\)", "Analysis samples", &materials_contents);
        if writing_samples.is_empty() {
            writing_samples = parse_question(
                r"Please submit at least one writing sample \(and no more tha(?s:.*)three\) that you feel represent(?s:.*)you(?s:.*)providin(?s:.*)links if(?s:.*)necessary\.",
                "Analysis samples",
                &materials_contents,
            );
            if writing_samples.is_empty() {
                writing_samples = parse_question(r"Writing samples", "Analysis samples", &materials_contents);

                if writing_samples.is_empty() {
                    writing_samples =
                        parse_question(r"Writing sample\(s\)", "Code and/or design sample", &materials_contents);
                }
            }
        }
        self.writing_samples = writing_samples;

        let mut analysis_samples =
            parse_question(r"Analysis sample\(s\)$", "Presentation samples", &materials_contents);
        if analysis_samples.is_empty() {
            analysis_samples = parse_question(
                r"please recount a(?s:.*)incident(?s:.*)which you analyzed syste(?s:.*)misbehavior(?s:.*)including as much technical detail as you can recall\.",
                "Presentation samples",
                &materials_contents,
            );
            if analysis_samples.is_empty() {
                analysis_samples = parse_question(r"Analysis samples", "Presentation samples", &materials_contents);
            }
        }
        self.analysis_samples = analysis_samples;

        let mut presentation_samples =
            parse_question(r"Presentation sample\(s\)", "Questionnaire", &materials_contents);
        if presentation_samples.is_empty() {
            presentation_samples = parse_question(
                r"I(?s:.*)you don’t have a publicl(?s:.*)available presentation(?s:.*)pleas(?s:.*)describe a topic on which you have presented in th(?s:.*)past\.",
                "Questionnaire",
                &materials_contents,
            );
            if presentation_samples.is_empty() {
                presentation_samples = parse_question(r"Presentation samples", "Questionnaire", &materials_contents);
            }
        }
        self.presentation_samples = presentation_samples;

        let mut exploratory_samples = parse_question(r"Exploratory sample\(s\)", "Questionnaire", &materials_contents);
        if exploratory_samples.is_empty() {
            exploratory_samples = parse_question(
                r"What’s an example o(?s:.*)something that you needed to explore, reverse engineer, decipher or otherwise figure out a(?s:.*)part of a program or project and how did you do it\? Please provide as much detail as you ca(?s:.*)recall\.",
                "Questionnaire",
                &materials_contents,
            );
            if exploratory_samples.is_empty() {
                exploratory_samples = parse_question(r"Exploratory samples", "Questionnaire", &materials_contents);
            }
        }
        self.exploratory_samples = exploratory_samples;

        self.question_technically_challenging = parse_question(
            QUESTION_TECHNICALLY_CHALLENGING,
            QUESTION_WORK_PROUD_OF,
            &materials_contents,
        );
        self.question_proud_of = parse_question(QUESTION_WORK_PROUD_OF, QUESTION_HAPPIEST_CAREER, &materials_contents);
        self.question_happiest = parse_question(
            QUESTION_HAPPIEST_CAREER,
            QUESTION_UNHAPPIEST_CAREER,
            &materials_contents,
        );
        self.question_unhappiest = parse_question(
            QUESTION_UNHAPPIEST_CAREER,
            QUESTION_VALUE_REFLECTED,
            &materials_contents,
        );
        self.question_value_reflected =
            parse_question(QUESTION_VALUE_REFLECTED, QUESTION_VALUE_VIOLATED, &materials_contents);
        self.question_value_violated =
            parse_question(QUESTION_VALUE_VIOLATED, QUESTION_VALUES_IN_TENSION, &materials_contents);
        self.question_values_in_tension =
            parse_question(QUESTION_VALUES_IN_TENSION, QUESTION_WHY_OXIDE, &materials_contents);
        self.question_why_oxide = parse_question(QUESTION_WHY_OXIDE, "", &materials_contents);
    }

    /// Cleanup the applicants phone.
    fn cleanup_phone(&mut self) {
        // Cleanup and parse the phone number and country code.
        let mut phone = self
            .phone
            .replace(" ", "")
            .replace("-", "")
            .replace("+", "")
            .replace("(", "")
            .replace(")", "");

        let location = self.location.to_string();
        let mut country = phonenumber::country::US;
        if (location.to_lowercase().contains("uk")
            || location.to_lowercase().contains("london")
            || location.to_lowercase().contains("ipswich")
            || location.to_lowercase().contains("united kingdom")
            || location.to_lowercase().contains("england"))
            && phone.starts_with("44")
        {
            country = phonenumber::country::GB;
        } else if (location.to_lowercase().contains("czech republic") || location.to_lowercase().contains("prague"))
            && phone.starts_with("420")
        {
            country = phonenumber::country::CZ;
        } else if location.to_lowercase().contains("turkey") && phone.starts_with("90") {
            country = phonenumber::country::TR;
        } else if location.to_lowercase().contains("sweden") && phone.starts_with("46") {
            country = phonenumber::country::SE;
        } else if (location.to_lowercase().contains("mumbai")
            || location.to_lowercase().contains("india")
            || location.to_lowercase().contains("bangalore"))
            && phone.starts_with("91")
        {
            country = phonenumber::country::IN;
        } else if location.to_lowercase().contains("brazil") {
            country = phonenumber::country::BR;
        } else if location.to_lowercase().contains("belgium") {
            country = phonenumber::country::BE;
        } else if location.to_lowercase().contains("romania") && phone.starts_with("40") {
            country = phonenumber::country::RO;
        } else if location.to_lowercase().contains("nigeria") {
            country = phonenumber::country::NG;
        } else if location.to_lowercase().contains("austria") {
            country = phonenumber::country::AT;
        } else if location.to_lowercase().contains("australia") && phone.starts_with("61") {
            country = phonenumber::country::AU;
        } else if location.to_lowercase().contains("sri lanka") && phone.starts_with("94") {
            country = phonenumber::country::LK;
        } else if location.to_lowercase().contains("slovenia") && phone.starts_with("386") {
            country = phonenumber::country::SI;
        } else if location.to_lowercase().contains("france") && phone.starts_with("33") {
            country = phonenumber::country::FR;
        } else if location.to_lowercase().contains("netherlands") && phone.starts_with("31") {
            country = phonenumber::country::NL;
        } else if location.to_lowercase().contains("taiwan") {
            country = phonenumber::country::TW;
        } else if location.to_lowercase().contains("new zealand") {
            country = phonenumber::country::NZ;
        } else if location.to_lowercase().contains("maragno") || location.to_lowercase().contains("italy") {
            country = phonenumber::country::IT;
        } else if location.to_lowercase().contains("nairobi") || location.to_lowercase().contains("kenya") {
            country = phonenumber::country::KE;
        } else if location.to_lowercase().contains("dubai") {
            country = phonenumber::country::AE;
        } else if location.to_lowercase().contains("poland") {
            country = phonenumber::country::PL;
        } else if location.to_lowercase().contains("portugal") {
            country = phonenumber::country::PT;
        } else if location.to_lowercase().contains("berlin") || location.to_lowercase().contains("germany") {
            country = phonenumber::country::DE;
        } else if location.to_lowercase().contains("benin") && phone.starts_with("229") {
            country = phonenumber::country::BJ;
        } else if location.to_lowercase().contains("israel") {
            country = phonenumber::country::IL;
        } else if location.to_lowercase().contains("spain") {
            country = phonenumber::country::ES;
        }

        let db = &phonenumber::metadata::DATABASE;
        let metadata = db.by_id(country.as_ref()).unwrap();
        let country_code = metadata.id().to_string().to_lowercase();

        // Get the last ten character of the string.
        if let Ok(phone_number) = phonenumber::parse(Some(country), phone.to_string()) {
            if !phone_number.is_valid() {
                info!("phone number is invalid: `{}`", phone);
            }

            phone = format!("{}", phone_number.format().mode(phonenumber::Mode::International));
        }
        self.phone = phone;
        self.country_code = country_code;
    }

    /// Get the human duration of time since the application was submitted.
    pub fn human_duration(&self) -> HumanTime {
        let mut dur = self.submitted_time - Utc::now();
        if dur.num_seconds() > 0 {
            dur = -dur;
        }

        HumanTime::from(dur)
    }

    /// Get the applicant's information in the form of the body of an email for a
    /// company wide notification that we received a new application.
    fn as_company_notification_email(&self) -> String {
        let time = self.human_duration();

        let mut msg = format!(
            "## Applicant Information for {}

Submitted {}
Name: {}
Email: {}",
            self.role, time, self.name, self.email
        );

        if !self.location.is_empty() {
            msg += &format!("\nLocation: {}", self.location);
        }
        if !self.phone.is_empty() {
            msg += &format!("\nPhone: {}", self.phone);
        }

        if !self.github.is_empty() {
            msg += &format!(
                "\nGitHub: {} (https://github.com/{})",
                self.github,
                self.github.trim_start_matches('@')
            );
        }
        if !self.gitlab.is_empty() {
            msg += &format!(
                "\nGitLab: {} (https://gitlab.com/{})",
                self.gitlab,
                self.gitlab.trim_start_matches('@')
            );
        }
        if !self.linkedin.is_empty() {
            msg += &format!("\nLinkedIn: {}", self.linkedin);
        }
        if !self.portfolio.is_empty() {
            msg += &format!("\nPortfolio: {}", self.portfolio);
        }
        if !self.website.is_empty() {
            msg += &format!("\nWebsite: {}", self.website);
        }

        if !self.scoring_form_url.is_empty() {
            msg += &format!("\n\nScoring form url: {}\n", self.scoring_form_url);
        }

        msg += &format!(
            "\nResume: {}
Oxide Candidate Materials: {}
Interested in: {}

## Reminder

The applicants Airtable \
             is at: https://airtable-applicants.corp.oxide.computer
",
            self.resume,
            self.materials,
            self.interested_in.join(", ")
        );

        msg
    }
}

fn get_color_based_on_status(s: &str) -> String {
    let status = crate::applicant_status::Status::from_str(s).unwrap();

    let color = match status {
        crate::applicant_status::Status::NextSteps => crate::colors::Colors::Blue,
        crate::applicant_status::Status::Deferred => crate::colors::Colors::Red,
        crate::applicant_status::Status::Declined => crate::colors::Colors::Red,
        crate::applicant_status::Status::Hired => crate::colors::Colors::Green,
        crate::applicant_status::Status::GivingOffer => crate::colors::Colors::Green,
        crate::applicant_status::Status::Contractor => crate::colors::Colors::Green,
        crate::applicant_status::Status::NeedsToBeTriaged => crate::colors::Colors::Yellow,
        crate::applicant_status::Status::Interviewing => crate::colors::Colors::Blue,
        crate::applicant_status::Status::Onboarding => crate::colors::Colors::Green,
    };

    color.to_string()
}

/// Convert the applicant into a Slack message.
impl From<NewApplicant> for FormattedMessage {
    fn from(item: NewApplicant) -> Self {
        let time = item.human_duration();

        let mut status_msg = item.role.to_string();

        if !item.interested_in.is_empty() {
            // Make sure we don't repeat the same string as the role for no reason.
            let joined = item.interested_in.join(",");
            if joined != item.role {
                status_msg += &format!(" | {}", joined);
            }
        }

        if !item.status.is_empty() {
            status_msg += &format!(" | *{}*", item.status);
        }

        status_msg += &format!(" | applied {}", time);

        let mut values_msg = "".to_string();
        if !item.value_reflected.is_empty() {
            values_msg += &format!("values reflected: *{}*", item.value_reflected);
        }
        if !item.value_violated.is_empty() {
            values_msg += &format!(" | violated: *{}*", item.value_violated);
        }
        for (k, tension) in item.values_in_tension.iter().enumerate() {
            if k == 0 {
                values_msg += &format!(" | in tension: *{}*", tension);
            } else {
                values_msg += &format!(" *& {}*", tension);
            }
        }
        if values_msg.is_empty() {
            values_msg = "values not yet populated".to_string();
        }

        let mut intro_msg = format!("*{}*  <mailto:{}|{}>", item.name, item.email, item.email,);
        if !item.location.is_empty() {
            intro_msg += &format!("  {}", item.location);
        }

        let mut info_msg = format!("<{}|resume> | <{}|materials>", item.resume, item.materials,);
        if !item.phone.is_empty() {
            info_msg += &format!(" | <tel:{}|{}>", item.phone, item.phone);
        }
        if !item.github.is_empty() {
            info_msg += &format!(
                " | <https://github.com/{}|github:{}>",
                item.github.trim_start_matches('@'),
                item.github,
            );
        }
        if !item.gitlab.is_empty() {
            info_msg += &format!(
                " | <https://gitlab.com/{}|gitlab:{}>",
                item.gitlab.trim_start_matches('@'),
                item.gitlab,
            );
        }
        if !item.linkedin.is_empty() {
            info_msg += &format!(" | <{}|linkedin>", item.linkedin,);
        }
        if !item.portfolio.is_empty() {
            info_msg += &format!(" | <{}|portfolio>", item.portfolio,);
        }
        if !item.portfolio_pdf.is_empty() {
            info_msg += &format!(" | <{}|portfolio pdf>", item.portfolio_pdf,);
        }
        if !item.website.is_empty() {
            info_msg += &format!(" | <{}|website>", item.website,);
        }

        FormattedMessage {
            channel: Default::default(),
            blocks: Default::default(),
            attachments: vec![MessageAttachment {
                color: get_color_based_on_status(&item.status),
                author_icon: Default::default(),
                author_link: Default::default(),
                author_name: Default::default(),
                fallback: Default::default(),
                fields: Default::default(),
                footer: Default::default(),
                footer_icon: Default::default(),
                image_url: Default::default(),
                pretext: Default::default(),
                text: Default::default(),
                thumb_url: Default::default(),
                title: Default::default(),
                title_link: Default::default(),
                ts: Default::default(),
                blocks: vec![
                    MessageBlock {
                        block_type: MessageBlockType::Section,
                        text: Some(MessageBlockText {
                            text_type: MessageType::Markdown,
                            text: intro_msg,
                        }),
                        elements: Default::default(),
                        accessory: Default::default(),
                        block_id: Default::default(),
                        fields: Default::default(),
                    },
                    MessageBlock {
                        block_type: MessageBlockType::Context,
                        elements: vec![slack_chat_api::BlockOption::MessageBlockText(MessageBlockText {
                            text_type: MessageType::Markdown,
                            text: info_msg,
                        })],
                        text: Default::default(),
                        accessory: Default::default(),
                        block_id: Default::default(),
                        fields: Default::default(),
                    },
                    MessageBlock {
                        block_type: MessageBlockType::Context,
                        elements: vec![slack_chat_api::BlockOption::MessageBlockText(MessageBlockText {
                            text_type: MessageType::Markdown,
                            text: values_msg,
                        })],
                        text: Default::default(),
                        accessory: Default::default(),
                        block_id: Default::default(),
                        fields: Default::default(),
                    },
                    MessageBlock {
                        block_type: MessageBlockType::Context,
                        elements: vec![slack_chat_api::BlockOption::MessageBlockText(MessageBlockText {
                            text_type: MessageType::Markdown,
                            text: status_msg,
                        })],
                        text: Default::default(),
                        accessory: Default::default(),
                        block_id: Default::default(),
                        fields: Default::default(),
                    },
                ],
            }],
        }
    }
}

impl From<Applicant> for FormattedMessage {
    fn from(item: Applicant) -> Self {
        let new: NewApplicant = item.into();
        new.into()
    }
}

impl Applicant {
    pub async fn send_slack_notification(&self, db: &Database, company: &Company) -> Result<()> {
        let n: NewApplicant = self.into();
        n.send_slack_notification(db, company).await
    }

    pub async fn send_slack_notification_background_check_status_changed(
        &self,
        db: &Database,
        company: &Company,
    ) -> Result<()> {
        let mut msg: FormattedMessage = self.clone().into();
        // Set the channel.
        msg.channel = company.slack_channel_applicants.to_string();

        let update = MessageBlock {
            block_type: MessageBlockType::Section,
            text: Some(MessageBlockText {
                text_type: MessageType::Markdown,
                text: format!(
                    "background check status is now `{}`",
                    self.criminal_background_check_status
                ),
            }),
            elements: Default::default(),
            accessory: Default::default(),
            block_id: Default::default(),
            fields: Default::default(),
        };

        // Make the new block be the second thing.
        msg.attachments[0].blocks.insert(1, update);

        // Post the message.
        company.post_to_slack_channel(db, &msg).await?;

        Ok(())
    }

    pub async fn send_slack_notification_status_changed(&self, db: &Database, company: &Company) -> Result<()> {
        let mut msg: FormattedMessage = self.clone().into();
        // Set the channel.
        msg.channel = company.slack_channel_applicants.to_string();

        let update = MessageBlock {
            block_type: MessageBlockType::Section,
            text: Some(MessageBlockText {
                text_type: MessageType::Markdown,
                text: format!("status is now `{}`", self.status),
            }),
            elements: Default::default(),
            accessory: Default::default(),
            block_id: Default::default(),
            fields: Default::default(),
        };

        // Make the new block be the second thing.
        msg.attachments[0].blocks.insert(1, update);

        // Post the message.
        company.post_to_slack_channel(db, &msg).await?;

        Ok(())
    }

    pub async fn send_slack_notification_start_date_changed(&self, db: &Database, company: &Company) -> Result<()> {
        if self.start_date.is_none() {
            // Return early, we don't care.
            return Ok(());
        }

        let mut msg: FormattedMessage = self.clone().into();
        // Set the channel.
        msg.channel = company.slack_channel_applicants.to_string();

        let start_date = self.start_date.unwrap();
        let dur = start_date - Utc::now().date().naive_utc();
        let human_date = HumanTime::from(dur);

        let update = MessageBlock {
            block_type: MessageBlockType::Section,
            text: Some(MessageBlockText {
                text_type: MessageType::Markdown,
                text: format!("start date is now `{}`, {}", start_date.format("%F"), human_date),
            }),
            elements: Default::default(),
            accessory: Default::default(),
            block_id: Default::default(),
            fields: Default::default(),
        };

        // Make the new block be the second thing.
        msg.attachments[0].blocks.insert(1, update);

        // Post the message.
        company.post_to_slack_channel(db, &msg).await?;

        Ok(())
    }

    pub async fn send_slack_notification_docusign_offer_status_changed(
        &self,
        db: &Database,
        company: &Company,
    ) -> Result<()> {
        let mut msg: FormattedMessage = self.clone().into();
        // Set the channel.
        msg.channel = company.slack_channel_applicants.to_string();

        let update = MessageBlock {
            block_type: MessageBlockType::Section,
            text: Some(MessageBlockText {
                text_type: MessageType::Markdown,
                text: format!("docusign offer status is now `{}`", self.docusign_envelope_status),
            }),
            elements: Default::default(),
            accessory: Default::default(),
            block_id: Default::default(),
            fields: Default::default(),
        };

        // Make the new block be the second thing.
        msg.attachments[0].blocks.insert(1, update);

        // Post the message.
        company.post_to_slack_channel(db, &msg).await?;

        Ok(())
    }

    pub async fn send_slack_notification_docusign_piia_status_changed(
        &self,
        db: &Database,
        company: &Company,
    ) -> Result<()> {
        let mut msg: FormattedMessage = self.clone().into();
        // Set the channel.
        msg.channel = company.slack_channel_applicants.to_string();

        let update = MessageBlock {
            block_type: MessageBlockType::Section,
            text: Some(MessageBlockText {
                text_type: MessageType::Markdown,
                text: format!(
                    "docusign employee agreements status is now `{}`",
                    self.docusign_piia_envelope_status
                ),
            }),
            elements: Default::default(),
            accessory: Default::default(),
            block_id: Default::default(),
            fields: Default::default(),
        };

        // Make the new block be the second thing.
        msg.attachments[0].blocks.insert(1, update);

        // Post the message.
        company.post_to_slack_channel(db, &msg).await?;

        Ok(())
    }

    /// Update an applicant's status based on dates, interviews, etc.
    pub async fn update_status(&mut self, db: &Database, company: &Company) -> Result<()> {
        let mut send_notification = false;

        // If we know they have more than 1 interview AND their current status is "next steps",
        // THEN we can mark the applicant as in the "interviewing" state.
        if self.interviews.len() > 1
            && (self.status == crate::applicant_status::Status::NextSteps.to_string()
                || self.status == crate::applicant_status::Status::NeedsToBeTriaged.to_string())
        {
            send_notification = self.status != crate::applicant_status::Status::Interviewing.to_string();
            self.status = crate::applicant_status::Status::Interviewing.to_string();
        }

        // If their status is "Onboarding" and it is after their start date.
        // Set their status to "Hired".
        if (self.status == crate::applicant_status::Status::Onboarding.to_string()
            || self.status == crate::applicant_status::Status::GivingOffer.to_string())
            && self.start_date.is_some()
            && self.start_date.unwrap() <= Utc::now().date().naive_utc()
        {
            // We shouldn't also check if we have an employee for the user, only if the employee had
            // been hired and left.
            // TODO: Have a status for if the employee was hired but then left the company.
            send_notification = self.status != crate::applicant_status::Status::Hired.to_string();
            self.status = crate::applicant_status::Status::Hired.to_string();
        }

        if send_notification {
            // Update the database first just in case.
            self.update(db).await?;
            self.send_slack_notification_status_changed(db, company).await?;
        }

        Ok(())
    }

    /// Update the interviews start and end time, if we have it.
    pub fn update_interviews_start_end_time(&mut self, db: &Database) {
        // If we have interviews for them, let's update the interviews_started and
        // interviews_completed times.
        if self.interviews.is_empty() || self.airtable_record_id.is_empty() {
            // Return early we don't care.
            return;
        }

        // Since our interviews length is at least one, we must have at least one interview.
        // Let's query the interviews for this candidate.
        let data = applicant_interviews::dsl::applicant_interviews
            .filter(applicant_interviews::dsl::applicant.contains(vec![self.airtable_record_id.to_string()]))
            .order_by(applicant_interviews::dsl::start_time.asc())
            .load::<ApplicantInterview>(&db.conn())
            .unwrap();
        // Probably a better way to do this using first and last, but whatever.
        for (index, r) in data.iter().enumerate() {
            if index == 0 {
                // We have the first record.
                // Let's update the started time.
                self.interviews_started = Some(r.start_time);
                // We continue here so we don't accidentally set the
                // completed_time if we only have one record.
                continue;
            }
            if index == data.len() - 1 {
                // We are on the last record.
                // Let's update the completed time.
                self.interviews_completed = Some(r.end_time);
                break;
            }
        }
    }

    /// Update applicant reviews counts.
    pub async fn update_reviews_scoring(&mut self, db: &Database) -> Result<()> {
        self.keep_fields_from_airtable(db).await;

        // If they have no reviews, eff it.
        if self.link_to_reviews.is_empty() {
            // Return early.
            return Ok(());
        }

        // Zero out the values for the scores.
        self.scoring_evaluations_count = 0;
        self.scoring_enthusiastic_yes_count = 0;
        self.scoring_yes_count = 0;
        self.scoring_pass_count = 0;
        self.scoring_no_count = 0;
        self.scoring_not_applicable_count = 0;
        self.scoring_insufficient_experience_count = 0;
        self.scoring_inapplicable_experience_count = 0;
        self.scoring_job_function_yet_needed_count = 0;
        self.scoring_underwhelming_materials_count = 0;

        // Create the Airtable client.
        let company = Company::get_by_id(db, self.cio_company_id)?;
        let airtable = company.authenticate_airtable(&company.airtable_base_id_hiring);

        if self.status == crate::applicant_status::Status::Onboarding.to_string()
            || self.status == crate::applicant_status::Status::Hired.to_string()
        {
            // Let's iterate over the reviews.
            for record_id in &self.link_to_reviews {
                // Get the record.
                // TODO: get these from the database.
                let record: airtable_api::Record<crate::applicant_reviews::ApplicantReview> = airtable
                    .get_record(crate::airtable::AIRTABLE_REVIEWS_TABLE, record_id)
                    .await?;

                // Set the values if they are not empty.
                // TODO: actually do the majority if they differ in value but for now YOLO.
                if !record.fields.value_reflected.is_empty() {
                    self.value_reflected = record.fields.value_reflected.to_string();
                }
                if !record.fields.value_violated.is_empty() {
                    self.value_violated = record.fields.value_violated.to_string();
                }
                if !record.fields.values_in_tension.is_empty() {
                    self.values_in_tension = record.fields.values_in_tension.clone();
                }

                // Delete the record from the reviews Airtable.
                airtable
                    .delete_record(crate::airtable::AIRTABLE_REVIEWS_TABLE, record_id)
                    .await?;

                // Delete the record if it exists in the Database.
                let r = ApplicantReview::get_by_id(db, record.fields.id)?;
                // Delete it.
                r.delete(db).await?;
            }

            // We already zero-ed out the values for the scores, now we return early.
            // We don't want people who join to know their scores.
            self.update(db).await?;
            return Ok(());
        }

        // Let's iterate over the reviews.
        for record_id in &self.link_to_reviews {
            // Get the record.
            // TODO: get these from the database.
            let record: airtable_api::Record<crate::applicant_reviews::ApplicantReview> = airtable
                .get_record(crate::airtable::AIRTABLE_REVIEWS_TABLE, record_id)
                .await
                .unwrap();

            // Set the values if they are not empty.
            // TODO: actually do the majority if they differ in value but for now YOLO.
            if !record.fields.value_reflected.is_empty() {
                self.value_reflected = record.fields.value_reflected.to_string();
            }
            if !record.fields.value_violated.is_empty() {
                self.value_violated = record.fields.value_violated.to_string();
            }
            if !record.fields.values_in_tension.is_empty() {
                self.values_in_tension = record.fields.values_in_tension.clone();
            }

            // Add the scoring count.
            self.scoring_evaluations_count += 1;

            // Up the scores for the relevant evaluations.
            if record.fields.evaluation.to_lowercase().starts_with("emphatic yes:") {
                self.scoring_enthusiastic_yes_count += 1;
            }
            if record.fields.evaluation.to_lowercase().starts_with("yes:") {
                self.scoring_yes_count += 1;
            }
            if record.fields.evaluation.to_lowercase().starts_with("pass:") {
                self.scoring_pass_count += 1;
            }
            if record.fields.evaluation.to_lowercase().starts_with("no:") {
                self.scoring_no_count += 1;
            }
            if record.fields.evaluation.to_lowercase().starts_with("n/a:") {
                self.scoring_not_applicable_count += 1;
            }

            // Add in the rationale.
            if record
                .fields
                .evaluation
                .to_lowercase()
                .starts_with("insufficient experience")
            {
                self.scoring_insufficient_experience_count += 1;
            }
            if record
                .fields
                .evaluation
                .to_lowercase()
                .starts_with("inapplicable experience")
            {
                self.scoring_inapplicable_experience_count += 1;
            }
            if record
                .fields
                .evaluation
                .to_lowercase()
                .starts_with("job function not yet needed")
            {
                self.scoring_job_function_yet_needed_count += 1;
            }
            if record
                .fields
                .evaluation
                .to_lowercase()
                .starts_with("underwhelming materials")
            {
                self.scoring_underwhelming_materials_count += 1;
            }

            // If we don't already have the review in reviewers completed,
            // add them.
            if !self.scorers_completed.contains(&record.fields.reviewer) {
                self.scorers_completed.push(record.fields.reviewer.to_string());
            }

            // If this reviewer was assigned, remove them since they completed scoring.
            if self.scorers.contains(&record.fields.reviewer) {
                let index = self.scorers.iter().position(|r| *r == record.fields.reviewer).unwrap();
                self.scorers.remove(index);
            }
        }

        // Update the record.
        self.update(db).await?;

        Ok(())
    }

    /// Get the human duration of time since the application was submitted.
    pub fn human_duration(&self) -> HumanTime {
        let mut dur = self.submitted_time - Utc::now();
        if dur.num_seconds() > 0 {
            dur = -dur;
        }

        HumanTime::from(dur)
    }

    /// Send an invite to the applicant to do a background check.
    pub async fn send_background_check_invitation(&mut self, db: &Database) -> Result<()> {
        // Keep the fields from Airtable we need just in case they changed.
        self.keep_fields_from_airtable(db).await;

        let company = self.company(db)?;
        let checkr_auth = company.authenticate_checkr();
        if checkr_auth.is_none() {
            // Return early.
            return Ok(());
        }

        let checkr = checkr_auth.unwrap();

        // Check if we already sent them an invitation.
        let candidates = checkr.list_candidates().await?;
        for candidate in candidates {
            if candidate.email == self.email {
                // Check if we already have sent their invitation.
                if self.criminal_background_check_status.is_empty() {
                    // Create an invitation for the candidate.
                    checkr.create_invitation(&candidate.id, "premium_criminal").await?;

                    // Update the database.
                    self.criminal_background_check_status = "requested".to_string();

                    self.update(db).await?;

                    self.send_slack_notification_background_check_status_changed(db, &company)
                        .await?;

                    info!("sent background check invitation to: {}", self.email);
                }
                // We can return early they already exist as a candidate and we have sent them an
                // invite.
                return Ok(());
            }
        }

        // Create a new candidate for the applicant in checkr.
        let candidate = checkr.create_candidate(&self.email).await?;

        // Create an invitation for the candidate.
        checkr.create_invitation(&candidate.id, "premium_criminal").await?;

        // Update the database.
        self.criminal_background_check_status = "requested".to_string();

        self.update(db).await?;

        self.send_slack_notification_background_check_status_changed(db, &company)
            .await?;

        info!("sent background check invitation to: {}", self.email);

        Ok(())
    }

    /// Send an email to a scorer that they are assigned to an applicant.
    pub async fn send_email_to_scorer(&self, scorer: &str, company: &Company) {
        // Initialize the SendGrid client.
        let sendgrid_client = SendGrid::new_from_env();

        // Send the message.
        sendgrid_client
            .mail_send()
            .send_plain_text(
                &format!("[applicants] Reviewing applicant {}", self.name),
                &self.as_scorer_email(),
                &[scorer.to_string()],
                &[],
                &[],
                &format!("careers@{}", company.gsuite_domain),
            )
            .await
            .unwrap();
    }

    /// Get the applicant's information in the form of the body of an email for a
    /// scorer email that they have been assigned to score the applicant.
    pub fn as_scorer_email(&self) -> String {
        let time = self.human_duration();

        let mut msg = format!(
            "You have been assigned to review the applicant: {}

Role: {}
Submitted: {}
Name: {}
Email: {}",
            self.name, self.role, time, self.name, self.email
        );

        if !self.location.is_empty() {
            msg += &format!("\nLocation: {}", self.location);
        }
        if !self.phone.is_empty() {
            msg += &format!("\nPhone: {}", self.phone);
        }

        if !self.github.is_empty() {
            msg += &format!(
                "\nGitHub: {} (https://github.com/{})",
                self.github,
                self.github.trim_start_matches('@')
            );
        }
        if !self.gitlab.is_empty() {
            msg += &format!(
                "\nGitLab: {} (https://gitlab.com/{})",
                self.gitlab,
                self.gitlab.trim_start_matches('@')
            );
        }
        if !self.linkedin.is_empty() {
            msg += &format!("\nLinkedIn: {}", self.linkedin);
        }
        if !self.portfolio.is_empty() {
            msg += &format!("\nPortfolio: {}", self.portfolio);
        }
        if !self.website.is_empty() {
            msg += &format!("\nWebsite: {}", self.website);
        }

        msg += &format!(
            "\nResume: {}
Oxide Candidate Materials: {}
Scoring form: {}
Scoring form responses: {}

## \
             Reminder

The applicants Airtable is at: https://airtable-applicants.corp.oxide.computer\
             ",
            self.resume, self.materials, self.scoring_form_url, self.scoring_form_responses_url,
        );

        msg
    }

    pub async fn create_github_onboarding_issue(
        &self,
        db: &Database,
        github: &octorust::Client,
        configs_issues: &[octorust::types::IssueSimple],
    ) -> Result<()> {
        let company = self.company(db)?;

        // Make sure they have a start date.
        if self.start_date.is_none() {
            // Return early.
            return Ok(());
        }

        let owner = &company.github_org;
        let repo = "configs";

        let split = self.name.splitn(2, ' ');
        let parts: Vec<&str> = split.collect();
        let first_name = parts[0];
        let last_name = parts[1];

        // Let's check the user's database to see if we can give this person the
        // {first_name}@ email.
        let mut username = first_name.to_lowercase().to_string();
        let existing_user = User::get_from_db(db, company.id, username.to_string());
        if existing_user.is_some() {
            username = format!("{}.{}", first_name.replace(' ', "-"), last_name.replace(' ', "-"));
        }
        // Make sure it's lowercase.
        username = username.to_lowercase();

        let label = "hiring".to_string();
        let title = format!("Onboarding: {}", self.name);
        let body = format!(
            r#"- [ ] Add to users.toml
- [ ] Add to matrix chat

Start Date: {}
Personal Email: {}
Twitter: [TWITTER HANDLE]
GitHub: {}
Phone: {}
Location: {}
cc @jessfraz

```
[users.{}]
first_name = '{}'
last_name = '{}'
username = '{}'
aliases = []
groups = [
    'all',
    'friends-of-oxide',
    'hardware',
    'manufacturing',
    'pci-sig',
]
recovery_email = '{}'
recovery_phone = '{}'
gender = ''
github = '{}'
chat = ''
aws_role = 'arn:aws:iam::128433874814:role/GSuiteSSO,arn:aws:iam::128433874814:saml-provider/GoogleApps'
department = ''
manager = ''
```"#,
            self.start_date.unwrap().format("%A, %B %-d, %C%y").to_string(),
            self.email,
            self.github,
            self.phone,
            self.location,
            username.replace('.', "-"),
            first_name,
            last_name,
            username,
            self.email,
            self.phone.replace('-', "").replace(' ', ""),
            self.github.replace('@', ""),
        );

        // Check if we already have an issue for this user.
        let issue = check_if_github_issue_exists(configs_issues, &self.name);

        // Check if their status is not onboarding, we only care about onboarding applicants.
        if self.status != crate::applicant_status::Status::Onboarding.to_string() {
            // If the issue exists and is opened, we need to close it.
            if let Some(i) = issue {
                if i.state != "open" {
                    // We only care if the issue is still opened.
                    return Ok(());
                }

                // Comment on the issue that this person is now set to a different status and we no
                // longer need the issue.
                github
                    .issues()
                    .create_comment(
                        owner,
                        repo,
                        i.number,
                        &octorust::types::PullsUpdateReviewRequest {
                            body: format!(
                                "Closing issue automatically since the applicant is now status: \
                                 `{}`
Notes:
> {}",
                                self.status, self.raw_status
                            ),
                        },
                    )
                    .await?;

                // Close the issue.
                github
                    .issues()
                    .update(
                        owner,
                        repo,
                        i.number,
                        &octorust::types::IssuesUpdateRequest {
                            title: Some(title.into()),
                            body: Default::default(),
                            assignee: "jessfraz".to_string(),
                            assignees: Default::default(),
                            labels: vec![label.into()],
                            milestone: Default::default(),
                            state: Some(octorust::types::State::Closed),
                        },
                    )
                    .await?;
            }

            // Return early.
            return Ok(());
        }

        // If we don't have a start date, return early.
        if self.start_date.is_none() {
            return Ok(());
        }

        // Create an issue for the applicant.
        if let Some(i) = issue {
            if i.state != "open" {
                // Make sure the issue is in the state of "open".
                github
                    .issues()
                    .update(
                        owner,
                        repo,
                        i.number,
                        &octorust::types::IssuesUpdateRequest {
                            title: Some(title.into()),
                            body: body.to_string(),
                            assignee: "jessfraz".to_string(),
                            assignees: Default::default(),
                            labels: vec![label.into()],
                            milestone: Default::default(),
                            state: Some(octorust::types::State::Open),
                        },
                    )
                    .await?;
            } else {
                // If the issue does not have any check marks.
                // Update it.
                let checkmark = "[x]".to_string();
                if !i.body.contains(&checkmark) {
                    github
                        .issues()
                        .update(
                            owner,
                            repo,
                            i.number,
                            &octorust::types::IssuesUpdateRequest {
                                title: Some(title.into()),
                                body: body.to_string(),
                                assignee: "jessfraz".to_string(),
                                assignees: Default::default(),
                                labels: vec![label.into()],
                                milestone: Default::default(),
                                state: Some(octorust::types::State::Open),
                            },
                        )
                        .await?;
                }
            }

            // Return early we don't want to update the issue because it will overwrite
            // any changes we made.
            return Ok(());
        }

        // Create the issue.
        github
            .issues()
            .create(
                owner,
                repo,
                &octorust::types::IssuesCreateRequest {
                    title: title.into(),
                    body,
                    assignee: "jessfraz".to_string(),
                    assignees: Default::default(),
                    labels: vec![label.into()],
                    milestone: Default::default(),
                },
            )
            .await?;

        info!("created onboarding issue for {}", self.email);

        Ok(())
    }

    /// Send an email to the applicant that we love them but they are too junior.
    pub async fn send_email_rejection_junior_but_we_love_you(&self, db: &Database) -> Result<()> {
        let company = self.company(db)?;
        // Initialize the SendGrid client.
        let sendgrid_client = SendGrid::new_from_env();

        // Send the message.
        sendgrid_client
            .mail_send()
            .send_plain_text(
                &format!("Thank you for your application, {}", self.name),
                &format!(
                    "Dear {},

Thank you for your application to join Oxide Computer Company. At this point
in time, we are focusing on hiring engineers with professional experience,
who have a track record of self-directed contributions to a team.

We are grateful you took the time to apply and put so much thought into
your candidate materials, we loved reading them. Although engineers at the
early stages of their career are unlikely to be a fit for us right now, we
are growing, and encourage you to consider re-applying in the future.

 We would absolutely love to work with you in the future and cannot wait for
that stage of the company!

All the best,
The Oxide Team",
                    self.name
                ),
                &[self.email.to_string()],
                &[format!("careers@{}", company.gsuite_domain)],
                &[],
                &format!("careers@{}", company.gsuite_domain),
            )
            .await?;

        Ok(())
    }

    /// Send an email to the applicant that they did not provide materials.
    pub async fn send_email_rejection_did_not_provide_materials(&self, db: &Database) -> Result<()> {
        let company = self.company(db)?;
        // Initialize the SendGrid client.
        let sendgrid_client = SendGrid::new_from_env();

        // Send the message.
        sendgrid_client
            .mail_send()
            .send_plain_text(
                &format!("Thank you for your application, {}", self.name),
                &format!(
                    "Dear {},

Unfortunately, we cannot accept it at this time since you failed to provide the
requested materials.

All the best,
The Oxide Team",
                    self.name
                ),
                &[self.email.to_string()],
                &[format!("careers@{}", company.gsuite_domain)],
                &[],
                &format!("careers@{}", company.gsuite_domain),
            )
            .await?;

        Ok(())
    }

    /// Send an email to the applicant about timing.
    pub async fn send_email_rejection_timing(&self, db: &Database) -> Result<()> {
        let company = self.company(db)?;
        // Initialize the SendGrid client.
        let sendgrid_client = SendGrid::new_from_env();

        // Send the message.
        sendgrid_client
            .mail_send()
            .send_plain_text(
                &format!("Thank you for your application, {}", self.name),
                &format!(
                    "Dear {},

We are so humbled by your application to join Oxide Computer Company. At this
stage of the company we are hyper-focused on certain areas of the stack and
when we need specific domain space experience such as yours, please engage
with us. Our roles will be updated as we need them.

We are grateful you took the time to apply and put so much thought into the
candidate materials, we loved reading them. We would absolutely love to work
with you in the future and cannot wait for that stage of the company!

All the best,
The Oxide Team",
                    self.name
                ),
                &[self.email.to_string()],
                &[format!("careers@{}", company.gsuite_domain)],
                &[],
                &format!("careers@{}", company.gsuite_domain),
            )
            .await?;

        Ok(())
    }
}

fn parse_question(q1: &str, q2: &str, materials_contents: &str) -> String {
    if materials_contents.is_empty() {
        Default::default()
    }

    let re = Regex::new(&(q1.to_owned() + r"(?s)(.*)" + q2)).unwrap();
    if let Some(q) = re.captures(materials_contents) {
        let val = q.get(1).unwrap();
        let s = val
            .as_str()
            .replace("________________", "")
            .replace("Oxide Candidate Materials: Technical Program Manager", "")
            .replace("Oxide Candidate Materials", "")
            .replace("Work sample(s)", "")
            .trim_start_matches(':')
            .trim()
            .to_string();

        if s.is_empty() {
            return Default::default();
        }

        return s;
    }

    Default::default()
}

/// Implement updating the Airtable record for an Applicant.
#[async_trait]
impl UpdateAirtableRecord<Applicant> for Applicant {
    async fn update_airtable_record(&mut self, record: Applicant) -> Result<()> {
        self.interviews = record.interviews;
        self.geocode_cache = record.geocode_cache;
        self.link_to_reviews = record.link_to_reviews;
        self.resume_contents = truncate(&self.resume_contents, 100000);
        self.materials_contents = truncate(&self.materials_contents, 100000);
        self.question_why_oxide = truncate(&self.question_why_oxide, 100000);

        Ok(())
    }
}

/// The data type for a Google Sheet applicant columns, we use this when
/// parsing the Google Sheets for applicants.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ApplicantSheetColumns {
    pub timestamp: usize,
    pub name: usize,
    pub email: usize,
    pub location: usize,
    pub phone: usize,
    pub github: usize,
    pub portfolio: usize,
    pub website: usize,
    pub linkedin: usize,
    pub resume: usize,
    pub materials: usize,
    pub status: usize,
    pub sent_email_received: usize,
    pub sent_email_follow_up: usize,
    pub value_reflected: usize,
    pub value_violated: usize,
    pub value_in_tension_1: usize,
    pub value_in_tension_2: usize,
    pub start_date: usize,
    pub interested_in: usize,
}

impl ApplicantSheetColumns {
    /// Parse the sheet columns from Google Sheets values.
    pub fn parse(values: &[Vec<String>]) -> Self {
        // Iterate over the columns.
        // TODO: make this less horrible
        let mut columns: ApplicantSheetColumns = Default::default();

        // Get the first row.
        let row = values.get(0).unwrap();

        for (index, col) in row.iter().enumerate() {
            let c = col.to_lowercase();

            if c.contains("timestamp") {
                columns.timestamp = index;
            }
            if c.contains("name") {
                columns.name = index;
            }
            if c.contains("email address") {
                columns.email = index;
            }
            if c.contains("location") {
                columns.location = index;
            }
            if c.contains("phone") {
                columns.phone = index;
            }
            if c.contains("github") {
                columns.github = index;
            }
            if c.contains("portfolio url") {
                columns.portfolio = index;
            }
            if c.contains("website") {
                columns.website = index;
            }
            if c.contains("linkedin profile url") {
                columns.linkedin = index;
            }
            if c.contains("resume") {
                columns.resume = index;
            }
            if c.contains("materials") {
                columns.materials = index;
            }
            if c.contains("status") {
                columns.status = index;
            }
            if c.contains("value reflected") {
                columns.value_reflected = index;
            }
            if c.contains("value violated") {
                columns.value_violated = index;
            }
            if c.contains("value in tension [1") {
                columns.value_in_tension_1 = index;
            }
            if c.contains("value in tension [2") {
                columns.value_in_tension_2 = index;
            }
            if c.contains("sent email that we received their application") {
                columns.sent_email_received = index;
            }
            if c.contains("have sent follow up email") {
                columns.sent_email_follow_up = index;
            }
            if c.contains("start date") {
                columns.start_date = index;
            }
            if c.contains("job descriptions are you interested in") {
                columns.interested_in = index;
            }
        }
        columns
    }
}

/// Get the contexts of a file in Google Drive by it's URL as a text string.
pub async fn get_file_contents(drive_client: &GoogleDrive, url: &str) -> Result<String> {
    let id = url
        .replace("https://drive.google.com/open?id=", "")
        .replace("https://drive.google.com/file/d/", "")
        .replace("/view", "");

    // Get information about the file.
    let drive_file = drive_client
        .files()
        .get(
            &id, false, // acknowledge_abuse
            "",    // include_permissions_for_view
            true,  // supports_all_drives
            true,  // supports_team_drives
        )
        .await?;
    let mime_type = drive_file.mime_type;
    let name = drive_file.name;

    let mut path = env::temp_dir();
    let mut output = env::temp_dir();

    let mut result: String = Default::default();

    if mime_type == "application/pdf" {
        // Get the PDF contents from Drive.
        let contents = drive_client.files().download_by_id(&id).await?;

        path.push(format!("{}.pdf", id));

        let mut file = fs::File::create(&path)?;
        file.write_all(&contents)?;

        result = read_pdf(&name, path.clone())?;
    } else if mime_type == "text/html" {
        let contents = drive_client.files().download_by_id(&id).await?;

        // Wrap lines at 80 characters.
        result = from_read(&contents[..], 80);
    } else if mime_type == "application/vnd.google-apps.document" {
        result = drive_client.files().get_contents_by_id(&id).await?;
    } else if name.ends_with(".7z") {
        // Get the ip contents from Drive.
        let contents = drive_client.files().download_by_id(&id).await?;

        path.push(format!("{}.7z", id));

        let mut file = fs::File::create(&path)?;
        file.write_all(&contents)?;

        output.push(id);

        // Create the output directory.
        fs::create_dir_all(&output)?;

        // Extract the text from the archive.
        let cmd_out = Command::new("7z")
            .args(&["x", &format!("-o{}", output.to_str().unwrap()), path.to_str().unwrap()])
            .output()?;
        info!("pz7ip output: {}", String::from_utf8(cmd_out.stdout)?);

        // Walk the output directory trying to find our file.
        for entry in WalkDir::new(&output).min_depth(1) {
            let entry = entry?;
            let path = entry.path();
            if is_materials(path.file_name().unwrap().to_str().unwrap()) {
                // Concatenate all the tar files into our result.
                result += &format!(
                    "====================== 7z file: {} ======================\n\n",
                    path.to_str()
                        .unwrap()
                        .replace(env::temp_dir().as_path().to_str().unwrap(), "")
                );
                if path.extension().unwrap() == "pdf" {
                    result += &read_pdf(&name, path.to_path_buf())?;
                } else {
                    result += &fs::read_to_string(&path)?;
                }
                result += "\n\n\n";
            }
        }
    } else if name.ends_with(".tgz") || name.ends_with(".tar.gz") {
        // Get the ip contents from Drive.
        let contents = drive_client.files().download_by_id(&id).await?;

        path.push(format!("{}.tar.gz", id));

        let mut file = fs::File::create(&path)?;
        file.write_all(&contents)?;

        let tar_gz = fs::File::open(&path)?;
        let tar = GzDecoder::new(tar_gz);
        let mut archive = Archive::new(tar);
        output.push(id);
        info!("unpacking tar gz: {:?} -> {:?}", path, output);
        archive.unpack(&output)?;

        // Walk the output directory trying to find our file.
        for entry in WalkDir::new(&output).min_depth(1) {
            let entry = entry?;
            let path = entry.path();
            if is_materials(path.file_name().unwrap().to_str().unwrap()) {
                // Concatenate all the tar files into our result.
                result += &format!(
                    "====================== tarball file: {} ======================\n\n",
                    path.to_str()
                        .unwrap()
                        .replace(env::temp_dir().as_path().to_str().unwrap(), "")
                );
                if path.extension().unwrap() == "pdf" {
                    result += &read_pdf(&name, path.to_path_buf())?;
                } else {
                    result += &fs::read_to_string(&path)?;
                }
                result += "\n\n\n";
            }
        }
    } else if name.ends_with(".tar") {
        // Get the ip contents from Drive.
        let contents = drive_client.files().download_by_id(&id).await?;

        path.push(format!("{}.tar", id));

        let mut file = fs::File::create(&path)?;
        file.write_all(&contents)?;

        // Unpack the tarball.
        let mut tar = Archive::new(fs::File::open(&path)?);
        output.push(id);
        info!("unpacking tarball: {:?} -> {:?}", path, output);
        tar.unpack(&output)?;

        // Walk the output directory trying to find our file.
        for entry in WalkDir::new(&output).min_depth(1) {
            let entry = entry?;
            let path = entry.path();
            if is_materials(path.file_name().unwrap().to_str().unwrap()) {
                // Concatenate all the tar files into our result.
                result += &format!(
                    "====================== tarball file: {} ======================\n\n",
                    path.to_str()
                        .unwrap()
                        .replace(env::temp_dir().as_path().to_str().unwrap(), "")
                );
                if path.extension().unwrap() == "pdf" {
                    result += &read_pdf(&name, path.to_path_buf())?;
                } else {
                    result += &fs::read_to_string(&path)?;
                }
                result += "\n\n\n";
            }
        }
    } else if name.ends_with(".zip") {
        // This is patrick :)
        // Get the ip contents from Drive.
        let contents = drive_client.files().download_by_id(&id).await?;

        path.push(format!("{}.zip", id));

        let mut file = fs::File::create(&path)?;
        file.write_all(&contents)?;
        file = fs::File::open(&path)?;

        // Unzip the file.
        let mut archive = zip::ZipArchive::new(file)?;
        for i in 0..archive.len() {
            match archive.by_index(i) {
                Ok(mut file) => {
                    output = env::temp_dir();
                    output.push("zip/");
                    output.push(file.name());

                    let comment = file.comment();
                    if !comment.is_empty() {
                        info!("zip file {} comment: {}", i, comment);
                    }

                    if (&*file.name()).ends_with('/') {
                        info!("zip file {} extracted to \"{}\"", i, output.as_path().display());
                        fs::create_dir_all(&output)?;
                    } else {
                        info!(
                            "zip file {} extracted to \"{}\" ({} bytes)",
                            i,
                            output.as_path().display(),
                            file.size()
                        );

                        if let Some(p) = output.parent() {
                            if !p.exists() {
                                fs::create_dir_all(&p)?;
                            }
                        }
                        let mut outfile = fs::File::create(&output)?;
                        copy(&mut file, &mut outfile)?;

                        let file_name = output.to_str().unwrap();
                        if !output.is_dir() && is_materials(file_name) {
                            // Concatenate all the zip files into our result.
                            result += &format!(
                                "====================== zip file: {} ======================\n\n",
                                output
                                    .as_path()
                                    .to_str()
                                    .unwrap()
                                    .replace(env::temp_dir().as_path().to_str().unwrap(), "")
                            );
                            if output.as_path().extension().unwrap() == "pdf" {
                                result += &read_pdf(&name, output.clone())?;
                            } else {
                                result += &fs::read_to_string(&output)?;
                            }
                            result += "\n\n\n";
                        }
                    }
                }
                Err(e) => {
                    warn!("error unwrapping materials name {}: {}", name, e);
                }
            }
        }
    } else if name.ends_with(".pptx") || name.ends_with(".jpg")
    // TODO: handle these formats
    {
        warn!(
            "unsupported doc format -- mime type: {}, name: {}, path: {}",
            mime_type,
            name,
            path.to_str().unwrap()
        );
    } else if name.ends_with(".rtf") {
        // Get the RTF contents from Drive.
        let contents = drive_client.files().download_by_id(&id).await?;

        path.push(format!("{}.rtf", id));

        let mut file = fs::File::create(&path)?;
        file.write_all(&contents)?;

        result = read_rtf(path.clone());
    } else if name.ends_with(".doc") {
        // Get the RTF contents from Drive.
        let contents = drive_client.files().download_by_id(&id).await?;

        path.push(format!("{}.doc", id));

        let mut file = fs::File::create(&path)?;
        file.write_all(&contents)?;

        result = read_doc(path.clone());
    } else {
        let contents = drive_client.files().download_by_id(&id).await?;
        path.push(name.to_string());

        let mut file = fs::File::create(&path)?;
        file.write_all(&contents)?;

        output.push(format!("{}.txt", id));

        match Command::new("pandoc")
            .args(&["-o", output.clone().to_str().unwrap(), path.to_str().unwrap()])
            .output()
        {
            Ok(_) => (),
            Err(e) => {
                warn!("pandoc failed: {}", e);
                return Ok("".to_string());
            }
        }
        result = fs::read_to_string(output.clone())?;
    }

    // Delete the temporary file, if it exists.
    for p in vec![path, output] {
        if p.exists() && !p.is_dir() {
            fs::remove_file(p)?;
        }
    }

    Ok(result.trim().to_string())
}

fn read_doc(path: std::path::PathBuf) -> String {
    // Extract the text from the DOC
    let cmd_output = Command::new("catdoc").args(&[path.to_str().unwrap()]).output().unwrap();

    let result = String::from_utf8(cmd_output.stdout).unwrap();

    // Delete the temporary file, if it exists.
    for p in vec![path] {
        if p.exists() && !p.is_dir() {
            fs::remove_file(p).unwrap();
        }
    }

    result
}

fn read_rtf(path: std::path::PathBuf) -> String {
    // Extract the text from the RTF
    let cmd_output = Command::new("unrtf")
        .args(&["--text", path.to_str().unwrap()])
        .output()
        .unwrap();

    let result = String::from_utf8(cmd_output.stdout).unwrap();

    // Delete the temporary file, if it exists.
    for p in vec![path] {
        if p.exists() && !p.is_dir() {
            fs::remove_file(p).unwrap();
        }
    }

    result
}

fn read_pdf(name: &str, path: std::path::PathBuf) -> Result<String> {
    let mut output = env::temp_dir();
    output.push("tempfile.txt");

    // Extract the text from the PDF
    let cmd_output = Command::new("pdftotext")
        .args(&["-enc", "UTF-8", path.to_str().unwrap(), output.to_str().unwrap()])
        .output()?;

    let result = match fs::read_to_string(output.clone()) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "running pdf2text failed: {} | name: {}, path: {}\nstdout: {}\nstderr: {}",
                e,
                name,
                path.as_path().display(),
                String::from_utf8(cmd_output.stdout)?,
                String::from_utf8(cmd_output.stderr)?,
            );

            "".to_string()
        }
    };

    // Delete the temporary file, if it exists.
    for p in vec![path, output] {
        if p.exists() && !p.is_dir() {
            fs::remove_file(p).unwrap();
        }
    }

    Ok(result)
}

pub fn get_tracking_sheets() -> Vec<&'static str> {
    vec![
        "18ZyWSX4jHY2FOlOhGwDuX3wXV48JnCdxtCq9aXC8cjk",
        "1BOeZTdSNixkJsVHwf3Z0LMVlaXsc_0J8Fsy9BkCa7XM",
    ]
}

pub fn get_sheets_map() -> BTreeMap<&'static str, &'static str> {
    let mut sheets: BTreeMap<&str, &str> = BTreeMap::new();
    sheets.insert("Engineering", "1FHA-otHCGwe5fCRpcl89MWI7GHiFfN3EWjO6K943rYA");
    sheets.insert(
        "Product Engineering and Design",
        "1VkRgmr_ZdR-y_1NJc8L0Iv6UVqKaZapt3T_Bq_gqPiI",
    );
    sheets.insert(
        "Technical Program Management",
        "1Z9sNUBW2z-Tlie0ci8xiet4Nryh-F0O82TFmQ1rQqlU",
    );
    sheets.insert("Operations Manager", "1S21W7ouI4qLeic4T71MGRL1Vk-ToqSQ6Z95GN-PT6Zc");

    sheets
}

pub fn get_role_from_sheet_id(sheet_id: &str) -> String {
    for (name, id) in get_sheets_map() {
        if *id == *sheet_id {
            return name.to_string();
        }
    }

    String::new()
}

// Sync the applicants with our database.
pub async fn refresh_db_applicants(db: &Database, company: &Company) -> Result<()> {
    if company.airtable_base_id_hiring.is_empty() {
        // Return early.
        return Ok(());
    }

    let github = company.authenticate_github()?;

    // Get all the hiring issues on the configs repository.
    let configs_issues = github
        .issues()
        .list_all_for_repo(
            &company.github_org,
            "configs",
            // milestone
            "",
            octorust::types::IssuesListState::All,
            // assignee
            "",
            // creator
            "",
            // mentioned
            "",
            // labels
            "hiring",
            // sort
            Default::default(),
            // direction
            Default::default(),
            // since
            None,
        )
        .await?;

    // Initialize the GSuite sheets client.
    let drive_client = company.authenticate_google_drive(db).await?;

    // Initialize the GSuite sheets client.
    let sheets_client = company.authenticate_google_sheets(db).await?;

    // Iterate over the Google sheets and create or update GitHub issues
    // depending on the application status.
    for (sheet_name, sheet_id) in get_sheets_map() {
        // Get the values in the sheet.
        let sheet_values = sheets_client
            .spreadsheets()
            .values_get(
                sheet_id,
                "Form Responses 1!A1:Z1000",
                sheets::types::DateTimeRenderOption::FormattedString,
                sheets::types::Dimension::Rows,
                sheets::types::ValueRenderOption::FormattedValue,
            )
            .await?;
        let values = sheet_values.values;

        if values.is_empty() {
            panic!(
                "unable to retrieve any data values from Google sheet {} {}",
                sheet_id, sheet_name
            );
        }

        // Parse the sheet columns.
        let columns = ApplicantSheetColumns::parse(&values);

        // Iterate over the rows.
        for (row_index, row) in values.iter().enumerate() {
            if row_index == 0 {
                // Continue the loop since we were on the header row.
                continue;
            } // End get header information.

            // Break the loop early if we reached an empty row.
            if row[columns.email].is_empty() {
                break;
            }

            // Parse the applicant out of the row information.
            let mut applicant = NewApplicant::parse_from_row_with_columns(sheet_name, sheet_id, &columns, row).await;
            applicant
                .expand(
                    db,
                    &drive_client,
                    &sheets_client,
                    columns.sent_email_received,
                    columns.sent_email_follow_up,
                    row_index + 1,
                )
                .await?;

            if !applicant.sent_email_received {
                // Send a company-wide email.
                applicant.send_email_internally(db).await?;
            }

            let new_applicant = applicant.upsert(db).await?;

            new_applicant
                .create_github_onboarding_issue(db, &github, &configs_issues)
                .await?;
        }
    }

    Ok(())
}

/// The data type for a Google Sheet applicant form columns, we use this when
/// parsing the Google Sheets for applicant forms where we leave our voting.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ApplicantFormSheetColumns {
    pub name: usize,
    pub email: usize,
    pub form_id: usize,
    pub form_url: usize,
    pub form_responses_url: usize,
    pub scorers_completed: usize,
}

impl ApplicantFormSheetColumns {
    fn new() -> Self {
        ApplicantFormSheetColumns {
            name: 0,
            email: 1,
            form_id: 2,
            form_url: 3,
            form_responses_url: 4,
            scorers_completed: 5,
        }
    }
}

pub fn get_reviewer_pool(db: &Database, company: &Company) -> Result<Vec<String>> {
    let users = Users::get_from_db(db, company.id)?;

    let mut reviewers: Vec<String> = Default::default();
    for user in users {
        if user.typev == "full-time"
            && user.username != "robert.keith"
            && user.username != "robert"
            && user.username != "keith"
            && user.username != "thomas"
            && user.username != "arjen"
        {
            reviewers.push(user.email);
        }
    }
    Ok(reviewers)
}

pub async fn update_applications_with_scoring_forms(db: &Database, company: &Company) -> Result<()> {
    if company.airtable_base_id_hiring.is_empty() {
        // Return early.
        return Ok(());
    }

    // Initialize the GSuite sheets client.
    let sheets_client = company.authenticate_google_sheets(db).await?;

    for sheet_id in get_tracking_sheets() {
        // Get the values in the sheet.
        let sheet_values = sheets_client
            .spreadsheets()
            .values_get(
                sheet_id,
                "Applicants to review!A1:G1000",
                sheets::types::DateTimeRenderOption::FormattedString,
                sheets::types::Dimension::Rows,
                sheets::types::ValueRenderOption::FormattedValue,
            )
            .await?;
        let values = sheet_values.values;

        if values.is_empty() {
            bail!(
                "unable to retrieve any data values from Google sheet for applicant forms {}",
                sheet_id
            );
        }

        // Parse the sheet columns.
        let columns = ApplicantFormSheetColumns::new();

        /*let mut reviewer_pool = get_reviewer_pool(db, company)?;

        // We'll assign reviewers randomly but attempt to produce roughly even loads
        // across reviewers. To do this, we shuffle the list of reviewers, and then
        // create a cycling iterator that produces that shuffled sequence forever.
        // Whenever we need a group of 5 interviewers, we'll just take the next 5.
        let mut rng = rand::thread_rng();
        reviewer_pool.shuffle(&mut rng);
        reviewer_pool = reviewer_pool.iter().cloned().cycle();*/

        // Iterate over the rows.
        for (_, row) in values.iter().enumerate() {
            if row[columns.email].is_empty() {
                // Break our loop we are in an empty row.
                break;
            }

            let email = row[columns.email].to_string();
            let form_id = row[columns.form_id].to_string();
            let form_url = row[columns.form_url].to_string();
            let form_responses_url = row[columns.form_responses_url].to_string();

            let mut scorers_completed: Vec<String> = vec![];
            if row.len() > columns.scorers_completed {
                let scorers_completed_string = row[columns.scorers_completed].to_string();
                let scorers_completed_str: Vec<&str> = scorers_completed_string.split(',').collect();
                for s in scorers_completed_str {
                    match User::get_from_db(
                        db,
                        company.id,
                        s.trim_end_matches(&company.gsuite_domain)
                            .trim_end_matches('@')
                            .to_string(),
                    ) {
                        Some(user) => {
                            scorers_completed.push(user.email);
                        }
                        None => {
                            info!("could not find user with email: {}", email);
                        }
                    }
                }
            }

            // Update each of the applicants.
            for (_, sheet_id) in get_sheets_map() {
                if let Ok(mut applicant) = applicants::dsl::applicants
                    .filter(applicants::dsl::email.eq(email.to_string()))
                    .filter(applicants::dsl::sheet_id.eq(sheet_id.to_string()))
                    .first::<Applicant>(&db.conn())
                {
                    // Try to get from airtable.
                    // This ensures if we had any one offs added in airtable that they stay intact.
                    if let Some(record) = applicant.get_existing_airtable_record(db).await {
                        applicant.scorers = record.fields.scorers;
                        applicant.interviews = record.fields.interviews;
                    }
                    applicant.scorers_completed = scorers_completed.clone();

                    // Remove anyone from the scorers if they have already completed their review.
                    for _ in 0..5 {
                        for (index, scorer) in applicant.scorers.clone().iter().enumerate() {
                            if applicant.scorers_completed.contains(scorer) {
                                applicant.scorers.remove(index);
                                // Break the loop since now the indexes are all off.
                                // The next cron run will catch it.
                                // TODO: make this better.
                                break;
                            }
                        }
                    }

                    // Make sure the status is "Needs to be triaged".
                    let status = crate::applicant_status::Status::from_str(&applicant.status);
                    if status != Ok(crate::applicant_status::Status::NeedsToBeTriaged) {
                        // Update the applicant in the database.
                        applicant.update(db).await?;

                        // Continue we don't care.
                        continue;
                    }

                    applicant.scoring_form_id = form_id.to_string();
                    applicant.scoring_form_url = form_url.to_string();
                    applicant.scoring_form_responses_url = form_responses_url.to_string();

                    // See if we already have scorers assigned.
                    /*
                    DO NOT ASSIGN NEW SCORERS RANDOMLY.
                     if applicant.scorers.is_empty() || (applicant.scorers.len() + applicant.scorers_completed.len()) < 5 {
                         // Assign scorers and send email.
                         // Choose next five reviewers.
                         let mut random_five: Vec<String> = reviewer_pool.by_ref().take(5).collect();
                         applicant.scorers.append(&mut random_five);
                         // Remove any duplicates.
                         applicant.scorers.sort_unstable();
                         applicant.scorers.dedup();
                     }

                     // Remove anyone from the scorers if they have already completed their review.
                     for _ in 0..5 {
                         for (index, scorer) in applicant.scorers.clone().iter().enumerate() {
                             if applicant.scorers_completed.contains(scorer) {
                                 applicant.scorers.remove(index);
                                 // Break the loop since now the indexes are all off.
                                 // The next cron run will catch it.
                                 // TODO: make this better.
                                 break;
                             }
                         }
                     }*/

                    // Update the applicant in the database.
                    applicant.update(db).await?;
                }
            }
        }
    }

    Ok(())
}

pub async fn update_applications_with_scoring_results(db: &Database, company: &Company) -> Result<()> {
    if company.airtable_base_id_hiring.is_empty() {
        // Return early.
        return Ok(());
    }

    // Initialize the GSuite sheets client.
    let sheets_client = company.authenticate_google_sheets(db).await?;

    for sheet_id in get_tracking_sheets() {
        // Get the values in the sheet.
        let sheet_values = sheets_client
            .spreadsheets()
            .values_get(
                sheet_id,
                "Responses!A1:R1000",
                sheets::types::DateTimeRenderOption::FormattedString,
                sheets::types::Dimension::Rows,
                sheets::types::ValueRenderOption::FormattedValue,
            )
            .await?;
        let values = sheet_values.values;

        if values.is_empty() {
            bail!(
                "unable to retrieve any data values from Google sheet for applicant form \
                 responses {}",
                sheet_id
            );
        }

        // Iterate over the rows.
        for (row_index, row) in values.iter().enumerate() {
            if row_index == 0 {
                // We are on the header row.
                continue;
            }
            if row[0].is_empty() {
                // Break our loop we are in an empty row.
                break;
            }

            let email = row[1].to_string();

            // Parse the scoring results.
            let scoring_evaluations_count = row[2].parse::<i32>().unwrap_or(0);
            let scoring_enthusiastic_yes_count = row[3].parse::<i32>().unwrap_or(0);
            let scoring_yes_count = row[4].parse::<i32>().unwrap_or(0);
            let scoring_pass_count = row[5].parse::<i32>().unwrap_or(0);
            let scoring_no_count = row[6].parse::<i32>().unwrap_or(0);
            let scoring_not_applicable_count = row[7].parse::<i32>().unwrap_or(0);
            let mut scoring_insufficient_experience_count = 0;
            let mut scoring_inapplicable_experience_count = 0;
            let mut scoring_job_function_yet_needed_count = 0;
            let mut scoring_underwhelming_materials_count = 0;
            let mut value_reflected = "".to_string();
            let mut value_violated = "".to_string();
            let mut values_in_tension: Vec<String> = vec![];

            if row.len() >= 10 {
                scoring_insufficient_experience_count = row[9].parse::<i32>().unwrap_or(0);
                scoring_inapplicable_experience_count = row[10].parse::<i32>().unwrap_or(0);
                scoring_job_function_yet_needed_count = row[11].parse::<i32>().unwrap_or(0);
                scoring_underwhelming_materials_count = row[12].parse::<i32>().unwrap_or(0);

                // Parse the values.
                value_reflected = row[14].to_lowercase().to_string();
                if value_reflected == "n/a" {
                    value_reflected = "".to_string();
                }
                value_violated = row[15].to_lowercase().to_string();
                if value_violated == "n/a" {
                    value_violated = "".to_string();
                }
                let value_in_tension_1 = row[16].to_lowercase().to_string();
                if value_in_tension_1 != "n/a" && !value_in_tension_1.trim().is_empty() {
                    values_in_tension.push(value_in_tension_1);
                }
                let value_in_tension_2 = row[17].to_lowercase().to_string();
                if value_in_tension_2 != "n/a" && !value_in_tension_2.trim().is_empty() {
                    values_in_tension.push(value_in_tension_2);
                }
                values_in_tension.sort();
            }

            // Update each of the applicants.
            for (_, sheet_id) in get_sheets_map() {
                if let Ok(mut applicant) = applicants::dsl::applicants
                    .filter(applicants::dsl::email.eq(email.to_string()))
                    .filter(applicants::dsl::sheet_id.eq(sheet_id.to_string()))
                    .first::<Applicant>(&db.conn())
                {
                    if applicant.status == crate::applicant_status::Status::Onboarding.to_string()
                        || applicant.status == crate::applicant_status::Status::Hired.to_string()
                    {
                        // Zero out the values for the scores.
                        applicant.scoring_evaluations_count = 0;
                        applicant.scoring_enthusiastic_yes_count = 0;
                        applicant.scoring_yes_count = 0;
                        applicant.scoring_pass_count = 0;
                        applicant.scoring_no_count = 0;
                        applicant.scoring_not_applicable_count = 0;
                        applicant.scoring_insufficient_experience_count = 0;
                        applicant.scoring_inapplicable_experience_count = 0;
                        applicant.scoring_job_function_yet_needed_count = 0;
                        applicant.scoring_underwhelming_materials_count = 0;
                    } else {
                        applicant.scoring_evaluations_count = scoring_evaluations_count;
                        applicant.scoring_enthusiastic_yes_count = scoring_enthusiastic_yes_count;
                        applicant.scoring_yes_count = scoring_yes_count;
                        applicant.scoring_pass_count = scoring_pass_count;
                        applicant.scoring_no_count = scoring_no_count;
                        applicant.scoring_not_applicable_count = scoring_not_applicable_count;
                        applicant.scoring_insufficient_experience_count = scoring_insufficient_experience_count;
                        applicant.scoring_inapplicable_experience_count = scoring_inapplicable_experience_count;
                        applicant.scoring_job_function_yet_needed_count = scoring_job_function_yet_needed_count;
                        applicant.scoring_underwhelming_materials_count = scoring_underwhelming_materials_count;
                    }

                    applicant.value_reflected = value_reflected.to_string();
                    applicant.value_violated = value_violated.to_string();
                    applicant.values_in_tension = values_in_tension.clone();

                    // Update the applicant in the database.
                    applicant.update(db).await?;
                }
            }
        }
    }

    // Ensure anyone with the status of "Onboarding" or "Hired" gets their scores zero-ed out.
    let applicants = applicants::dsl::applicants
        .filter(
            applicants::dsl::status
                .eq(crate::applicant_status::Status::Onboarding.to_string())
                .or(applicants::dsl::status.eq(crate::applicant_status::Status::Hired.to_string())),
        )
        .load::<Applicant>(&db.conn())?;
    for mut applicant in applicants {
        // Zero out the values for the scores.
        applicant.scoring_evaluations_count = 0;
        applicant.scoring_enthusiastic_yes_count = 0;
        applicant.scoring_yes_count = 0;
        applicant.scoring_pass_count = 0;
        applicant.scoring_no_count = 0;
        applicant.scoring_not_applicable_count = 0;
        applicant.scoring_insufficient_experience_count = 0;
        applicant.scoring_inapplicable_experience_count = 0;
        applicant.scoring_job_function_yet_needed_count = 0;
        applicant.scoring_underwhelming_materials_count = 0;

        // Update the applicant in the database.
        applicant.update(db).await?;
    }

    Ok(())
}

fn is_materials(file_name: &str) -> bool {
    file_name.ends_with("responses.pdf")
        || (file_name.starts_with("Oxide Candidate Materials") && file_name.ends_with(".pdf"))
        || (file_name.contains("Oxide_Candidate_Materials") && file_name.ends_with(".pdf"))
        || file_name.ends_with("Oxide Candidate Materials.pdf")
        || file_name.ends_with("Oxide Candidate Materials.pdf.pdf")
        || file_name.ends_with("OxideQuestions.pdf")
        || file_name.ends_with("oxide-computer-candidate-materials.pdf")
        || file_name.ends_with("Questionnaire.pdf")
        || file_name.ends_with("questionnaire.md")
        || file_name.ends_with("Questionairre.pdf")
        || file_name.ends_with("Operations Manager.pdf")
        || file_name.ends_with("README.md")
}

pub async fn refresh_background_checks(db: &Database, company: &Company) -> Result<()> {
    if company.airtable_base_id_hiring.is_empty() {
        // Return early.
        return Ok(());
    }

    // Initialize the Checker client.
    let checkr_auth = company.authenticate_checkr();
    if checkr_auth.is_none() {
        // Return early.
        return Ok(());
    }

    let checkr = checkr_auth.unwrap();

    // List the candidates.
    let candidates = checkr.list_candidates().await?;
    for candidate in candidates {
        // Try to match the candidate based on their email.
        // Try for all the sheet_ids.
        for (_, sheet_id) in get_sheets_map() {
            // Match on their email or their name.
            // TODO: check with the new way.
            // TODO: name is working for now but might want to make it more fuzzy in the future.
            // This could be problematic if we have two John Smiths join in the same week.
            if let Ok(mut applicant) = applicants::dsl::applicants
                .filter(
                    applicants::dsl::email
                        .eq(candidate.email.to_string())
                        .or(applicants::dsl::name.eq(format!("{} {}", candidate.first_name, candidate.last_name))),
                )
                .filter(applicants::dsl::sheet_id.eq(sheet_id.to_string()))
                .filter(applicants::dsl::status.eq(crate::applicant_status::Status::Onboarding.to_string()))
                .first::<Applicant>(&db.conn())
            {
                for report_id in &candidate.report_ids {
                    // Get the report for the candidate.
                    let report = checkr.get_report(report_id).await.unwrap();

                    let mut send_notification = false;

                    // Set the status for the report.
                    if report.package.contains("premium_criminal") {
                        send_notification = applicant.criminal_background_check_status != report.status;

                        applicant.criminal_background_check_status = report.status.to_string();
                    }
                    if report.package.contains("motor_vehicle") {
                        applicant.motor_vehicle_background_check_status = report.status.to_string();
                    }

                    // Update the applicant.
                    applicant.update(db).await?;

                    if send_notification {
                        applicant
                            .send_slack_notification_background_check_status_changed(db, company)
                            .await?;
                    }
                }
            } else {
                info!(
                    "could not find applicant with email {} in sheet_id {}",
                    candidate.email, sheet_id
                );
            }
        }
    }

    Ok(())
}

/// The data type for a ApplicantReviewer.
#[db {
    new_struct_name = "ApplicantReviewer",
    airtable_base = "hiring",
    airtable_table = "AIRTABLE_REVIEWER_LEADERBOARD_TABLE",
    match_on = {
        "email" = "String",
    },
}]
#[derive(Debug, Insertable, AsChangeset, PartialEq, Clone, JsonSchema, Deserialize, Serialize)]
#[table_name = "applicant_reviewers"]
pub struct NewApplicantReviewer {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        serialize_with = "airtable_api::user_format_as_string::serialize",
        deserialize_with = "airtable_api::user_format_as_string::deserialize"
    )]
    pub email: String,
    #[serde(default)]
    pub evaluations: i32,
    #[serde(default)]
    pub emphatic_yes: i32,
    #[serde(default)]
    pub yes: i32,
    #[serde(default)]
    pub pass: i32,
    #[serde(default)]
    pub no: i32,
    #[serde(default)]
    pub not_applicable: i32,
    /// The CIO company ID.
    #[serde(default)]
    pub cio_company_id: i32,
}

/// Implement updating the Airtable record for an ApplicantReviewer.
#[async_trait]
impl UpdateAirtableRecord<ApplicantReviewer> for ApplicantReviewer {
    async fn update_airtable_record(&mut self, _record: ApplicantReviewer) -> Result<()> {
        Ok(())
    }
}

pub async fn update_applicant_reviewers_leaderboard(db: &Database, company: &Company) -> Result<()> {
    if company.airtable_base_id_hiring.is_empty() {
        // Return early.
        return Ok(());
    }

    // Initialize the GSuite sheets client.
    let sheets_client = company.authenticate_google_sheets(db).await?;

    let sheet_id = "1BOeZTdSNixkJsVHwf3Z0LMVlaXsc_0J8Fsy9BkCa7XM";

    // Get the values in the sheet.
    let sheet_values = sheets_client
        .spreadsheets()
        .values_get(
            sheet_id,
            "Leaderboard!A1:R1000",
            sheets::types::DateTimeRenderOption::FormattedString,
            sheets::types::Dimension::Rows,
            sheets::types::ValueRenderOption::FormattedValue,
        )
        .await?;
    let values = sheet_values.values;

    if values.is_empty() {
        bail!(
            "unable to retrieve any data values from Google sheet for reviewer leaderboard {}",
            sheet_id
        );
    }

    // Iterate over the rows.
    for (row_index, row) in values.iter().enumerate() {
        if row_index == 0 {
            // We are on the header row.
            continue;
        }
        if row[0].is_empty() {
            // Break our loop we are in an empty row.
            break;
        }

        let email = row[0].to_string();

        // Parse the scoring results.
        let evaluations = row[1].parse::<i32>().unwrap_or(0);
        let emphatic_yes = row[2].parse::<i32>().unwrap_or(0);
        let yes = row[3].parse::<i32>().unwrap_or(0);
        let pass = row[4].parse::<i32>().unwrap_or(0);
        let no = row[5].parse::<i32>().unwrap_or(0);
        let not_applicable = row[6].parse::<i32>().unwrap_or(0);

        match User::get_from_db(
            db,
            company.id,
            email
                .trim_end_matches(&company.gsuite_domain)
                .trim_end_matches('@')
                .to_string(),
        ) {
            Some(user) => {
                let reviewer = NewApplicantReviewer {
                    name: user.full_name(),
                    email,
                    evaluations,
                    emphatic_yes,
                    yes,
                    pass,
                    no,
                    not_applicable,
                    cio_company_id: user.cio_company_id,
                };

                // Upsert the applicant reviewer in the database.
                reviewer.upsert(db).await?;
            }
            None => {
                info!("could not find user with email: {}", email);
            }
        }
    }

    Ok(())
}

pub async fn refresh_docusign_for_applicants(db: &Database, company: &Company) -> Result<()> {
    if company.airtable_base_id_hiring.is_empty() {
        // Return early.
        return Ok(());
    }

    // Authenticate DocuSign.
    let dsa = company.authenticate_docusign(db).await;
    if let Err(e) = dsa {
        if e.to_string().contains("no token") {
            // Return early, this company does not use Zoom.
            return Ok(());
        }

        bail!("authenticating docusign failed: {}", e);
    }
    let ds = dsa.unwrap();

    // Get the template we need.
    let offer_template_id = get_docusign_template_id(&ds, DOCUSIGN_OFFER_TEMPLATE).await;
    let piia_template_id = get_docusign_template_id(&ds, DOCUSIGN_PIIA_TEMPLATE).await;

    // TODO: we could actually query the DB by status, but whatever.
    let applicants = Applicants::get_from_db(db, company.id)?;

    // Iterate over the applicants and find any that have the status: giving offer.
    for mut applicant in applicants {
        applicant
            .do_docusign_offer(db, &ds, &offer_template_id, company)
            .await?;

        applicant.do_docusign_piia(db, &ds, &piia_template_id, company).await?;
    }

    Ok(())
}

pub async fn get_docusign_template_id(ds: &DocuSign, name: &str) -> String {
    let templates = ds.list_templates().await.unwrap();
    for template in templates {
        if template.name == name {
            return template.template_id;
        }
    }

    "".to_string()
}

impl Applicant {
    pub fn cleanup_linkedin(&mut self) {
        if self.linkedin.trim().is_empty() {
            self.linkedin = "".to_string();
            return;
        }

        // Cleanup linkedin link.
        self.linkedin = format!(
            "https://linkedin.com/{}",
            self.linkedin
                .trim_start_matches("https://linkedin.com/")
                .trim_start_matches("https://www.linkedin.com/")
                .trim_start_matches("http://linkedin.com/")
                .trim_start_matches("http://www.linkedin.com/")
                .trim_start_matches("www.linkedin.com/")
                .trim_start_matches("linkedin.com/")
                .trim()
        );
    }
    pub async fn set_lat_long(&mut self) {
        // Get the latitude and longitude if we don't already have it.
        if self.latitude != 0.0 && self.longitude != 0.0 {
            // Return early we alreaedy have lat and long set.
            return;
        }

        // Create the geocode client.
        let geocode = Geocode::new_from_env();
        // Attempt to get the lat and lng.
        match geocode.get(&self.location).await {
            Ok(result) => {
                let location = result.geometry.location;
                self.latitude = location.lat as f32;
                self.longitude = location.lng as f32;
            }
            Err(e) => {
                if !self.location.is_empty() {
                    warn!("could not get lat lng for location `{}`: {}", self.location, e);
                }
            }
        }
    }

    /// Send a rejection email if we need to.
    pub async fn send_email_follow_up_if_necessary(&mut self, db: &Database) -> Result<()> {
        // Send an email follow up if we should.
        if self.sent_email_follow_up {
            // We have already followed up with the candidate.
            // Let's return early.
            return Ok(());
        }

        // Get the status for the applicant.
        let status = crate::applicant_status::Status::from_str(&self.status).unwrap_or_default();

        if status != crate::applicant_status::Status::NeedsToBeTriaged
            && status != crate::applicant_status::Status::Declined
            && status != crate::applicant_status::Status::Deferred
        {
            // Just set that we have sent the email so that we don't do it again if we move to
            // next steps then interviews etc.
            // Only when it's not in "NeedsToBeTriaged", or we are about to defer or decline.
            // Mark the column as true not false.

            self.sent_email_follow_up = true;
            // Update the database.
            self.update(db).await?;
            // Return early, we don't actually want to send something, likely a member
            // of the Oxide team reached out directly.
            return Ok(());
        }

        if status != crate::applicant_status::Status::Declined && status != crate::applicant_status::Status::Deferred {
            // We want to return early, we only care about people who were deferred or declined.
            // So sent the folks in the triage home.
            // Above we sent home everyone else.
            return Ok(());
        }

        // Check if we have sent the follow up email to them.unwrap_or_default().
        if self.raw_status.contains("did not do materials") {
            // Send the email.
            self.send_email_rejection_did_not_provide_materials(db).await?;

            info!("sent email to {} tell them they did not do the materials", self.email);
        } else if self.raw_status.contains("junior") {
            // Send the email.
            self.send_email_rejection_junior_but_we_love_you(db).await?;

            info!(
                "sent email to {} tell them we can't hire them at this stage",
                self.email
            );
        } else {
            // Send the email.
            self.send_email_rejection_timing(db).await?;

            info!("sent email to {} tell them about timing", self.email);
        }

        // Mark the time we sent the email.
        self.rejection_sent_date_time = Some(Utc::now());

        self.sent_email_follow_up = true;
        // Update the database.
        self.update(db).await?;

        Ok(())
    }

    /// Expand the applicants materials and do any automation that needs to be done.
    pub async fn expand(&mut self, db: &Database, drive_client: &GoogleDrive) -> Result<()> {
        self.cleanup_phone();
        self.parse_github_gitlab();
        self.cleanup_linkedin();

        // Add the scoring url since now we should have an Airtable record id.
        // Since we are an Applicant.
        if !self.airtable_record_id.is_empty() {
            self.scoring_form_url = format!("https://apply.oxide.computer/review/{}", self.email);
        }

        // Check if we have sent them an email that we received their application.
        if !self.sent_email_received {
            // Send them an email.
            self.send_email_recieved_application_to_applicant(db).await?;
            self.sent_email_received = true;
            // Update it in the database just in case.
            self.update(db).await?;

            info!("sent email to {} that we received their application", self.email);
            // Send the email internally.
            self.send_email_internally(db).await?;
        }

        // Set the latitude and longitude if we don't already have it.
        self.set_lat_long().await;

        // Get the time seven days ago.
        let duration_from_now = Utc::now().signed_duration_since(self.submitted_time);

        // If the application is as new as the last week then parse all the contents.
        // This takes a long time so we skip all the others.
        if (duration_from_now < Duration::days(2)
            || (duration_from_now < Duration::days(20) && self.question_why_oxide.is_empty()))
            && self.status != crate::applicant_status::Status::Declined.to_string()
        {
            // Read the file contents.
            match get_file_contents(drive_client, &self.resume).await {
                Ok(r) => self.resume_contents = r,
                Err(e) => {
                    warn!("getting resume contents for applicant `{}` failed: {}", self.email, e);
                }
            }

            match get_file_contents(drive_client, &self.materials).await {
                Ok(r) => self.materials_contents = r,
                Err(e) => {
                    warn!(
                        "getting materials contents for applicant `{}` failed: {}",
                        self.email, e
                    );
                }
            }

            self.parse_materials();
        }

        Ok(())
    }

    /// Get the applicant's information in the form of the body of an email for a
    /// company wide notification that we received a new application.
    fn as_company_notification_email(&self) -> String {
        let time = self.human_duration();

        let mut msg = format!(
            "## Applicant Information for {}

Submitted {}
Name: {}
Email: {}",
            self.role, time, self.name, self.email
        );

        if !self.location.is_empty() {
            msg += &format!("\nLocation: {}", self.location);
        }
        if !self.phone.is_empty() {
            msg += &format!("\nPhone: {}", self.phone);
        }

        if !self.github.is_empty() {
            msg += &format!(
                "\nGitHub: {} (https://github.com/{})",
                self.github,
                self.github.trim_start_matches('@')
            );
        }
        if !self.gitlab.is_empty() {
            msg += &format!(
                "\nGitLab: {} (https://gitlab.com/{})",
                self.gitlab,
                self.gitlab.trim_start_matches('@')
            );
        }
        if !self.linkedin.is_empty() {
            msg += &format!("\nLinkedIn: {}", self.linkedin);
        }
        if !self.portfolio.is_empty() {
            msg += &format!("\nPortfolio: {}", self.portfolio);
        }
        if !self.website.is_empty() {
            msg += &format!("\nWebsite: {}", self.website);
        }

        if !self.scoring_form_url.is_empty() {
            msg += &format!("\n\nScoring form url: {}\n", self.scoring_form_url);
        }

        msg += &format!(
            "\nResume: {}
Oxide Candidate Materials: {}
Interested in: {}

## Reminder

The applicants Airtable \
             is at: https://airtable-applicants.corp.oxide.computer
",
            self.resume,
            self.materials,
            self.interested_in.join(", ")
        );

        msg
    }

    /// Send an email internally that we have a new application.
    async fn send_email_internally(&self, db: &Database) -> Result<()> {
        let company = self.company(db)?;
        // Initialize the SendGrid client.
        let sendgrid_client = SendGrid::new_from_env();

        // Send the message.
        sendgrid_client
            .mail_send()
            .send_plain_text(
                &format!("New {} Application: {}", self.role, self.name),
                &self.as_company_notification_email(),
                &[format!("applications@{}", company.gsuite_domain)],
                &[],
                &[],
                &format!("applications@{}", company.gsuite_domain),
            )
            .await?;

        Ok(())
    }

    /// Send an email to the applicant that we recieved their application.
    async fn send_email_recieved_application_to_applicant(&self, db: &Database) -> Result<()> {
        let company = self.company(db)?;
        // Initialize the SendGrid client.
        let sendgrid_client = SendGrid::new_from_env();

        // Send the message.
        sendgrid_client
            .mail_send()
            .send_plain_text(
                &format!(
                    "Oxide Computer Company {} Application Received for {}",
                    self.role, self.name
                ),
                &format!(
                    "Dear {},

Thank you for submitting your application materials! We really appreciate all
the time and thought everyone puts into their application. We will be in touch
within the next few weeks with more information. Just a heads up this could take
up to 4-6 weeks.

Sincerely,
  The Oxide Team",
                    self.name
                ),
                &[self.email.to_string()],
                &[format!("careers@{}", company.gsuite_domain)],
                &[],
                &format!("careers@{}", company.gsuite_domain),
            )
            .await?;

        Ok(())
    }

    /// Parse the questions from the materials.
    fn parse_materials(&mut self) {
        // Parse the samples and materials.
        let materials_contents = self.materials_contents.clone();
        let mut work_samples = parse_question(r"Work sample\(s\)", "Writing samples", &materials_contents);
        if work_samples.is_empty() {
            work_samples = parse_question(
                r"If(?s:.*)his work is entirely proprietary(?s:.*)please describe it as fully as y(?s:.*)can, providing necessary context\.",
                "Writing samples",
                &materials_contents,
            );
            if work_samples.is_empty() {
                // Try to parse work samples for TPM role.
                work_samples = parse_question(
                    r"What would you have done differently\?",
                    "Exploratory samples",
                    &materials_contents,
                );

                if work_samples.is_empty() {
                    work_samples = parse_question(
                        r"Some questions(?s:.*)o have in mind as you describe them:",
                        "Exploratory samples",
                        &materials_contents,
                    );

                    if work_samples.is_empty() {
                        work_samples = parse_question(r"Work samples", "Exploratory samples", &materials_contents);

                        if work_samples.is_empty() {
                            work_samples = parse_question(r"design sample\(s\)", "Questionnaire", &materials_contents);
                        }
                    }
                }
            }
        }
        self.work_samples = work_samples;

        let mut writing_samples = parse_question(r"Writing sample\(s\)", "Analysis samples", &materials_contents);
        if writing_samples.is_empty() {
            writing_samples = parse_question(
                r"Please submit at least one writing sample \(and no more tha(?s:.*)three\) that you feel represent(?s:.*)you(?s:.*)providin(?s:.*)links if(?s:.*)necessary\.",
                "Analysis samples",
                &materials_contents,
            );
            if writing_samples.is_empty() {
                writing_samples = parse_question(r"Writing samples", "Analysis samples", &materials_contents);

                if writing_samples.is_empty() {
                    writing_samples =
                        parse_question(r"Writing sample\(s\)", "Code and/or design sample", &materials_contents);
                }
            }
        }
        self.writing_samples = writing_samples;

        let mut analysis_samples =
            parse_question(r"Analysis sample\(s\)$", "Presentation samples", &materials_contents);
        if analysis_samples.is_empty() {
            analysis_samples = parse_question(
                r"please recount a(?s:.*)incident(?s:.*)which you analyzed syste(?s:.*)misbehavior(?s:.*)including as much technical detail as you can recall\.",
                "Presentation samples",
                &materials_contents,
            );
            if analysis_samples.is_empty() {
                analysis_samples = parse_question(r"Analysis samples", "Presentation samples", &materials_contents);
            }
        }
        self.analysis_samples = analysis_samples;

        let mut presentation_samples =
            parse_question(r"Presentation sample\(s\)", "Questionnaire", &materials_contents);
        if presentation_samples.is_empty() {
            presentation_samples = parse_question(
                r"I(?s:.*)you don’t have a publicl(?s:.*)available presentation(?s:.*)pleas(?s:.*)describe a topic on which you have presented in th(?s:.*)past\.",
                "Questionnaire",
                &materials_contents,
            );
            if presentation_samples.is_empty() {
                presentation_samples = parse_question(r"Presentation samples", "Questionnaire", &materials_contents);
            }
        }
        self.presentation_samples = presentation_samples;

        let mut exploratory_samples = parse_question(r"Exploratory sample\(s\)", "Questionnaire", &materials_contents);
        if exploratory_samples.is_empty() {
            exploratory_samples = parse_question(
                r"What’s an example o(?s:.*)something that you needed to explore, reverse engineer, decipher or otherwise figure out a(?s:.*)part of a program or project and how did you do it\? Please provide as much detail as you ca(?s:.*)recall\.",
                "Questionnaire",
                &materials_contents,
            );
            if exploratory_samples.is_empty() {
                exploratory_samples = parse_question(r"Exploratory samples", "Questionnaire", &materials_contents);
            }
        }
        self.exploratory_samples = exploratory_samples;

        self.question_technically_challenging = parse_question(
            QUESTION_TECHNICALLY_CHALLENGING,
            QUESTION_WORK_PROUD_OF,
            &materials_contents,
        );
        self.question_proud_of = parse_question(QUESTION_WORK_PROUD_OF, QUESTION_HAPPIEST_CAREER, &materials_contents);
        self.question_happiest = parse_question(
            QUESTION_HAPPIEST_CAREER,
            QUESTION_UNHAPPIEST_CAREER,
            &materials_contents,
        );
        self.question_unhappiest = parse_question(
            QUESTION_UNHAPPIEST_CAREER,
            QUESTION_VALUE_REFLECTED,
            &materials_contents,
        );
        self.question_value_reflected =
            parse_question(QUESTION_VALUE_REFLECTED, QUESTION_VALUE_VIOLATED, &materials_contents);
        self.question_value_violated =
            parse_question(QUESTION_VALUE_VIOLATED, QUESTION_VALUES_IN_TENSION, &materials_contents);
        self.question_values_in_tension =
            parse_question(QUESTION_VALUES_IN_TENSION, QUESTION_WHY_OXIDE, &materials_contents);
        self.question_why_oxide = parse_question(QUESTION_WHY_OXIDE, "", &materials_contents);
    }

    fn parse_github_gitlab(&mut self) {
        let mut github = "".to_string();
        let mut gitlab = "".to_string();
        if !self.github.trim().is_empty() {
            github = format!(
                "@{}",
                self.github
                    .trim()
                    .to_lowercase()
                    .trim_start_matches("https://github.com/")
                    .trim_start_matches("http://github.com/")
                    .trim_start_matches("https://www.github.com/")
                    .trim_start_matches("http://www.github.com/")
                    .trim_start_matches("www.github.com/")
                    .trim_start_matches("github.com/")
                    .trim_start_matches('@')
                    .replace("github.com/", "")
                    .trim_end_matches('/')
                    .trim_start_matches('/')
            )
            .trim()
            .to_string();

            if github == "@" || github == "@n/a" || github.contains("linkedin.com") {
                github = "".to_string();
            }

            // Some people put a gitlab URL in the github form input,
            // parse those accordingly.
            if github.contains("https://gitlab.com") {
                github = "".to_string();

                gitlab = format!(
                    "@{}",
                    self.github
                        .trim()
                        .to_lowercase()
                        .trim_start_matches("https://gitlab.com/")
                        .trim_start_matches('@')
                        .trim_end_matches('/')
                );
            }
        }

        self.github = github;
        self.gitlab = gitlab;
    }

    /// Cleanup the applicants phone.
    fn cleanup_phone(&mut self) {
        // Cleanup and parse the phone number and country code.
        let mut phone = self
            .phone
            .replace(" ", "")
            .replace("-", "")
            .replace("+", "")
            .replace("(", "")
            .replace(")", "");

        let location = self.location.to_string();
        let mut country = phonenumber::country::US;
        if (location.to_lowercase().contains("uk")
            || location.to_lowercase().contains("london")
            || location.to_lowercase().contains("ipswich")
            || location.to_lowercase().contains("united kingdom")
            || location.to_lowercase().contains("england"))
            && phone.starts_with("44")
        {
            country = phonenumber::country::GB;
        } else if (location.to_lowercase().contains("czech republic") || location.to_lowercase().contains("prague"))
            && phone.starts_with("420")
        {
            country = phonenumber::country::CZ;
        } else if location.to_lowercase().contains("turkey") && phone.starts_with("90") {
            country = phonenumber::country::TR;
        } else if location.to_lowercase().contains("sweden") && phone.starts_with("46") {
            country = phonenumber::country::SE;
        } else if (location.to_lowercase().contains("mumbai")
            || location.to_lowercase().contains("india")
            || location.to_lowercase().contains("bangalore"))
            && phone.starts_with("91")
        {
            country = phonenumber::country::IN;
        } else if location.to_lowercase().contains("brazil") {
            country = phonenumber::country::BR;
        } else if location.to_lowercase().contains("belgium") {
            country = phonenumber::country::BE;
        } else if location.to_lowercase().contains("romania") && phone.starts_with("40") {
            country = phonenumber::country::RO;
        } else if location.to_lowercase().contains("nigeria") {
            country = phonenumber::country::NG;
        } else if location.to_lowercase().contains("austria") {
            country = phonenumber::country::AT;
        } else if location.to_lowercase().contains("australia") && phone.starts_with("61") {
            country = phonenumber::country::AU;
        } else if location.to_lowercase().contains("sri lanka") && phone.starts_with("94") {
            country = phonenumber::country::LK;
        } else if location.to_lowercase().contains("slovenia") && phone.starts_with("386") {
            country = phonenumber::country::SI;
        } else if location.to_lowercase().contains("france") && phone.starts_with("33") {
            country = phonenumber::country::FR;
        } else if location.to_lowercase().contains("netherlands") && phone.starts_with("31") {
            country = phonenumber::country::NL;
        } else if location.to_lowercase().contains("taiwan") {
            country = phonenumber::country::TW;
        } else if location.to_lowercase().contains("new zealand") {
            country = phonenumber::country::NZ;
        } else if location.to_lowercase().contains("maragno") || location.to_lowercase().contains("italy") {
            country = phonenumber::country::IT;
        } else if location.to_lowercase().contains("nairobi") || location.to_lowercase().contains("kenya") {
            country = phonenumber::country::KE;
        } else if location.to_lowercase().contains("dubai") {
            country = phonenumber::country::AE;
        } else if location.to_lowercase().contains("poland") {
            country = phonenumber::country::PL;
        } else if location.to_lowercase().contains("portugal") {
            country = phonenumber::country::PT;
        } else if location.to_lowercase().contains("berlin") || location.to_lowercase().contains("germany") {
            country = phonenumber::country::DE;
        } else if location.to_lowercase().contains("benin") && phone.starts_with("229") {
            country = phonenumber::country::BJ;
        } else if location.to_lowercase().contains("israel") {
            country = phonenumber::country::IL;
        } else if location.to_lowercase().contains("spain") {
            country = phonenumber::country::ES;
        }

        let db = &phonenumber::metadata::DATABASE;
        let metadata = db.by_id(country.as_ref()).unwrap();
        let country_code = metadata.id().to_string().to_lowercase();

        // Get the last ten character of the string.
        if let Ok(phone_number) = phonenumber::parse(Some(country), phone.to_string()) {
            if !phone_number.is_valid() {
                info!("phone number is invalid: {}", phone);
            }

            phone = format!("{}", phone_number.format().mode(phonenumber::Mode::International));
        }
        self.phone = phone;
        self.country_code = country_code;
    }

    pub async fn do_docusign_offer(
        &mut self,
        db: &Database,
        ds: &DocuSign,
        template_id: &str,
        company: &Company,
    ) -> Result<()> {
        // Keep the fields from Airtable we need just in case they changed.
        self.keep_fields_from_airtable(db).await;

        // We look for "Onboarding" here as well since we want to make sure we can actually update
        // the data for the user.
        if self.status != crate::applicant_status::Status::GivingOffer.to_string()
            && self.status != crate::applicant_status::Status::Onboarding.to_string()
            && self.status != crate::applicant_status::Status::Hired.to_string()
        {
            // We can return early.
            return Ok(());
        }

        if self.docusign_envelope_id.is_empty()
            && self.status == crate::applicant_status::Status::GivingOffer.to_string()
        {
            info!(
                "applicant has status giving offer: {}, generating offer in docusign for them!",
                self.name
            );
            // We haven't sent their offer yet, so let's do that.
            // Let's create a new envelope for the user.
            let mut new_envelope: docusign::Envelope = Default::default();

            // Sent the status to `sent` so it sends.
            // To save it as a draft set the status as `created`.
            new_envelope.status = "sent".to_string();

            // Set the email subject.
            new_envelope.email_subject = DOCUSIGN_OFFER_SUBJECT.to_string();

            // Set the template id to that of our template.
            new_envelope.template_id = template_id.to_string();

            // Set the recipients of the template.
            // The first recipient needs to be the CEO (or whoever is going to do the mad lib for
            // the offer.
            // The second recipient needs to be the Applicant.
            new_envelope.template_roles = vec![
                docusign::TemplateRole {
                    name: "Steve Tuck".to_string(),
                    role_name: "CEO".to_string(),
                    email: format!("steve@{}", company.gsuite_domain),
                    signer_name: "Steve Tuck".to_string(),
                    routing_order: "1".to_string(),
                    // Make Steve's email notification different than the actual applicant.
                    email_notification: docusign::EmailNotification {
                        email_subject: format!("Complete the offer letter for {}", self.name),
                        email_body: format!(
                            "The status for the applicant, {}, has been changed to `Giving \
                             offer`. Therefore, we are sending you an offer letter to complete, \
                             as Jess calls, the 'Mad Libs'. GO COMPLETE THE MAD LIBS! After you \
                             finish, we will send the offer letter to {} at {} to sign and date! \
                             Thanks!",
                            self.name, self.name, self.email
                        ),
                        language: Default::default(),
                    },
                },
                docusign::TemplateRole {
                    name: self.name.to_string(),
                    role_name: "Applicant".to_string(),
                    email: self.email.to_string(),
                    signer_name: self.name.to_string(),
                    routing_order: "2".to_string(),
                    email_notification: docusign::EmailNotification {
                        email_subject: DOCUSIGN_OFFER_SUBJECT.to_string(),
                        email_body: "We are very excited to offer you a position at the Oxide \
                                     Computer Company!"
                            .to_string(),
                        language: Default::default(),
                    },
                },
                docusign::TemplateRole {
                    name: "Ruth Alexander".to_string(),
                    role_name: "HR".to_string(),
                    email: "ruth@mindsharegroup.com".to_string(),
                    signer_name: "Ruth Alexander".to_string(),
                    routing_order: "3".to_string(),
                    email_notification: docusign::EmailNotification {
                        email_subject: "Oxide Computer Company Offer Letter Signed".to_string(),
                        email_body: "Attached is a newly signed offer letter, please set up \
                                     benefits. Thank you!"
                            .to_string(),
                        language: Default::default(),
                    },
                },
            ];

            // Let's create the envelope.
            let envelope = ds.create_envelope(new_envelope.clone()).await?;

            // Set the id of the envelope.
            self.docusign_envelope_id = envelope.envelope_id.to_string();
            // Set the status of the envelope.
            self.docusign_envelope_status = envelope.status.to_string();

            // Update the applicant in the database.
            self.update(db).await?;

            // Send a slack notification that the docusign status changed.
            self.send_slack_notification_docusign_offer_status_changed(db, company)
                .await?;
        } else if !self.docusign_envelope_id.is_empty() {
            // We have sent their offer.
            // Let's get the status of the envelope in Docusign.
            let envelope = ds.get_envelope(&self.docusign_envelope_id).await?;

            self.update_applicant_from_docusign_offer_envelope(db, ds, envelope)
                .await?;
        }

        Ok(())
    }

    pub async fn update_applicant_from_docusign_offer_envelope(
        &mut self,
        db: &Database,
        ds: &DocuSign,
        envelope: docusign::Envelope,
    ) -> Result<()> {
        // Keep the fields from Airtable we need just in case they changed.
        self.keep_fields_from_airtable(db).await;

        let company = self.company(db)?;

        let send_notification = self.docusign_envelope_status != envelope.status;

        // Set the status in the database and airtable.
        self.docusign_envelope_status = envelope.status.to_string();
        self.offer_created = envelope.created_date_time;

        // If the document is completed, let's save it to Google Drive.
        if envelope.status != "completed" {
            // We will skip to the end and return early, only updating the status.
            self.update(db).await?;

            if send_notification {
                // Send a slack notification that the docusign status changed.
                self.send_slack_notification_docusign_offer_status_changed(db, &company)
                    .await?;
            }
            return Ok(());
        }

        // Set the completed time.
        self.offer_completed = envelope.completed_date_time;
        if self.status == crate::applicant_status::Status::GivingOffer.to_string() {
            // Since the status of the envelope is completed, let's set their status to "Onboarding".
            // Only do this if they are not already hired.
            self.status = crate::applicant_status::Status::Onboarding.to_string();
            // Update them in case something fails.
            self.update(db).await?;

            // Request their background check, if we have not already.
            if self.criminal_background_check_status.is_empty() {
                // Request the background check, since we previously have not requested one.
                self.send_background_check_invitation(db).await?;
            }
        }

        // Initialize the Google Drive client.
        let drive_client = company.authenticate_google_drive(db).await?;
        // Figure out where our directory is.
        // It should be in the shared drive : "Offer Letters"
        let shared_drive = drive_client.drives().get_by_name("Offer Letters").await?;
        let drive_id = shared_drive.id.to_string();

        // TODO: only save the documents if we don't already have them.
        for document in &envelope.documents {
            let mut bytes = base64::decode(&document.pdf_bytes).unwrap_or_default();
            // Check if we already have bytes to the data.
            if document.pdf_bytes.is_empty() {
                // Get the document from docusign.
                // In order to not "over excessively poll the API here, we need to sleep for 15
                // min before getting each of the documents.
                // https://developers.docusign.com/docs/esign-rest-api/esign101/rules-and-limits/
                //thread::sleep(std::time::Duration::from_secs(15));
                bytes = ds.get_document(&envelope.envelope_id, &document.id).await?.to_vec();
            }

            // Create the folder for our applicant with their name.
            let name_folder_id = drive_client
                .files()
                .create_folder(&shared_drive.id, "", &self.name)
                .await?;

            let mut filename = format!("{} - {}.pdf", self.name, document.name);
            if document.name.contains("Offer Letter") {
                filename = format!("{} - Offer.pdf", self.name);
            } else if document.name.contains("Summary") {
                filename = format!("{} - Offer - DocuSign Summary.pdf", self.name);
            } else if document.name.contains("Employee Mediation") || document.name.contains("Employee_Mediation") {
                filename = format!("{} - Mediation Agreement.pdf", self.name);
            } else if document.name.contains("Employee Proprietary") || document.name.contains("Employee_Proprietary") {
                filename = format!("{} - PIIA.pdf", self.name);
            }

            // Create or update the file in the google_drive.
            drive_client
                .files()
                .create_or_update(&drive_id, &name_folder_id, &filename, "application/pdf", &bytes)
                .await?;
            info!("uploaded completed file `{}` to drive", filename);
        }

        // In order to not "over excessively poll the API here, we need to sleep for 15
        // min before getting each of the documents.
        // https://developers.docusign.com/docs/esign-rest-api/esign101/rules-and-limits/
        //thread::sleep(std::time::Duration::from_secs(900));
        let form_data = ds.get_envelope_form_data(&self.docusign_envelope_id).await?;

        // Let's get the employee for the applicant.
        // We will match on their recovery email.
        let result = users::dsl::users
            .filter(
                users::dsl::recovery_email
                    .eq(self.email.to_string())
                    .and(users::dsl::cio_company_id.eq(company.id)),
            )
            .first::<User>(&db.conn());
        if result.is_ok() {
            let mut employee = result?;
            // Only do this if we don't have the employee's home address or start date.
            // This will help us to not override any changes then that are later made in gusto.
            if employee.home_address_street_1.is_empty() || employee.start_date == crate::utils::default_date() {
                // We have an employee, so we can update their data from the data in Docusign.

                for fd in form_data.clone() {
                    // Save the data to the employee who matches this applicant.
                    if fd.name == "Applicant's Street Address" {
                        employee.home_address_street_1 = fd.value.trim().to_string();
                    }
                    if fd.name == "Applicant's City" {
                        employee.home_address_city = fd.value.trim().to_string();
                    }
                    if fd.name == "Applicant's State" {
                        employee.home_address_state =
                            crate::states::StatesMap::match_abreev_or_return_existing(&fd.value);
                    }
                    if fd.name == "Applicant's Postal Code" {
                        employee.home_address_zipcode = fd.value.trim().to_string();
                    }
                    if fd.name == "Applicant's Country" {
                        employee.home_address_country = fd.value.trim().to_string();
                    }
                    if fd.name == "Start Date" {
                        let start_date = NaiveDate::parse_from_str(fd.value.trim(), "%m/%d/%Y")?;
                        employee.start_date = start_date;
                    }
                }
            }

            // Update the employee.
            employee.update(db).await?;
        }

        let mut send_notification_start_date = false;

        for fd in form_data {
            // TODO: we could somehow use the manager data here or above. The manager data is in
            // the docusign data.
            if fd.name == "Start Date" {
                let start_date = NaiveDate::parse_from_str(fd.value.trim(), "%m/%d/%Y")?;

                send_notification_start_date = self.start_date.is_none() || self.start_date.unwrap() != start_date;

                self.start_date = Some(start_date);
            }
        }

        self.update(db).await?;

        // Send the slack notification if we should.
        if send_notification {
            // Send a slack notification that the docusign status changed.
            self.send_slack_notification_docusign_offer_status_changed(db, &company)
                .await?;
        }

        if send_notification_start_date {
            // Send a slack notification that the start date changed.
            self.send_slack_notification_start_date_changed(db, &company).await?;
        }

        Ok(())
    }

    pub async fn do_docusign_piia(
        &mut self,
        db: &Database,
        ds: &DocuSign,
        template_id: &str,
        company: &Company,
    ) -> Result<()> {
        // Keep the fields from Airtable we need just in case they changed.
        self.keep_fields_from_airtable(db).await;

        // We look for "Onboarding" here as well since we want to make sure we can actually update
        // the data for the user.
        if self.status != crate::applicant_status::Status::GivingOffer.to_string()
            && self.status != crate::applicant_status::Status::Onboarding.to_string()
            && self.status != crate::applicant_status::Status::Hired.to_string()
        {
            // We can return early.
            return Ok(());
        }

        if self.docusign_piia_envelope_id.is_empty()
            && self.status == crate::applicant_status::Status::GivingOffer.to_string()
        {
            info!(
                "applicant has status giving offer: {}, generating employee agreements in docusign for them!",
                self.name
            );
            // We haven't sent their employee agreements yet, so let's do that.
            // Let's create a new envelope for the user.
            let mut new_envelope: docusign::Envelope = Default::default();

            // Sent the status to `sent` so it sends.
            // To save it as a draft set the status as `created`.
            new_envelope.status = "sent".to_string();

            // Set the email subject.
            new_envelope.email_subject = DOCUSIGN_PIIA_SUBJECT.to_string();

            // Set the template id to that of our template.
            new_envelope.template_id = template_id.to_string();

            // Set the recipients of the template.
            // The first recipient needs to be the CEO (or whoever is going to do the mad lib for
            // the offer.
            // The second recipient needs to be the Applicant.
            new_envelope.template_roles = vec![
                docusign::TemplateRole {
                    name: "Steve Tuck".to_string(),
                    role_name: "CEO".to_string(),
                    email: format!("steve@{}", company.gsuite_domain),
                    signer_name: "Steve Tuck".to_string(),
                    routing_order: "1".to_string(),
                    // Make Steve's email notification different than the actual applicant.
                    email_notification: docusign::EmailNotification {
                        email_subject: format!("Complete the employee agreements for {}", self.name),
                        email_body: format!(
                            "The status for the applicant, {}, has been changed to `Giving \
                             offer`. Therefore, we are sending you employee agreements to \
                             complete, as Jess calls, the 'Mad Libs'. GO COMPLETE THE MAD LIBS! \
                             After you finish, we will send the employee agreements to {} at {} \
                             to sign and date! Thanks!",
                            self.name, self.name, self.email
                        ),
                        language: Default::default(),
                    },
                },
                docusign::TemplateRole {
                    name: self.name.to_string(),
                    role_name: "Applicant".to_string(),
                    email: self.email.to_string(),
                    signer_name: self.name.to_string(),
                    routing_order: "2".to_string(),
                    email_notification: docusign::EmailNotification {
                        email_subject: DOCUSIGN_PIIA_SUBJECT.to_string(),
                        email_body: "Here are the PIIA (Employee Proprietary Information and \
                                     Invention Agreement) and Mediation documents. These do not \
                                     need to be returned with the offer letter (sent in a \
                                     separate DocuSign), but they need to be returned by your \
                                     start date. Please let Steve know if you have any questions!"
                            .to_string(),
                        language: Default::default(),
                    },
                },
                docusign::TemplateRole {
                    name: "Steve Tuck".to_string(),
                    role_name: "CEO (2)".to_string(),
                    email: format!("steve@{}", company.gsuite_domain),
                    signer_name: "Steve Tuck".to_string(),
                    routing_order: "3".to_string(),
                    // Make Steve's email notification different than the actual applicant.
                    email_notification: docusign::EmailNotification {
                        email_subject: format!("Sign the PIIA agreements for {}", self.name),
                        email_body: "This is the last step before we send to HR.".to_string(),
                        language: Default::default(),
                    },
                },
                docusign::TemplateRole {
                    name: "Ruth Alexander".to_string(),
                    role_name: "HR".to_string(),
                    email: "ruth@mindsharegroup.com".to_string(),
                    signer_name: "Ruth Alexander".to_string(),
                    routing_order: "4".to_string(),
                    email_notification: docusign::EmailNotification {
                        email_subject: "Oxide Computer Company Employee Agreements Signed".to_string(),
                        email_body: "Attached are newly signed employee agreements. Thank you!".to_string(),
                        language: Default::default(),
                    },
                },
            ];

            // Let's create the envelope.
            let envelope = ds.create_envelope(new_envelope.clone()).await?;

            // Set the id of the envelope.
            self.docusign_piia_envelope_id = envelope.envelope_id.to_string();
            // Set the status of the envelope.
            self.docusign_piia_envelope_status = envelope.status.to_string();

            // Update the applicant in the database.
            self.update(db).await?;

            // Send a slack notification that the docusign status changed.
            self.send_slack_notification_docusign_piia_status_changed(db, company)
                .await?;
        } else if !self.docusign_piia_envelope_id.is_empty() {
            // We have sent their employee agreements.
            // Let's get the status of the envelope in Docusign.
            let envelope = ds.get_envelope(&self.docusign_piia_envelope_id).await?;

            self.update_applicant_from_docusign_piia_envelope(db, ds, envelope)
                .await?;
        }

        Ok(())
    }

    pub async fn keep_fields_from_airtable(&mut self, db: &Database) {
        // Let's get the existing record from Airtable, so we can use it as the source
        // of truth for various things.
        let existing = self.get_existing_airtable_record(db).await.unwrap().fields;
        // We keep the scorers from Airtable in case someone assigned someone from the UI.
        self.scorers = existing.scorers.clone();
        // Keep the interviewers from Airtable since they are updated out of bound by Airtable.
        self.interviews = existing.interviews.clone();
        // Keep the reviews, since these are updated out of band by Airtable.
        self.link_to_reviews = existing.link_to_reviews;

        // We want to keep the status and status raw since we might have modified
        // it to move a candidate along in the process.
        self.status = existing.status.to_string();
        self.raw_status = existing.raw_status.to_string();

        // Mostly the start date will populate from docusign, but just in case they
        // are someone who worked remotely, we might have to manually set it.
        // If docusign is incorrect, make sure Airtable always has the source of truth.
        self.start_date = existing.start_date;
    }

    pub async fn update_applicant_from_docusign_piia_envelope(
        &mut self,
        db: &Database,
        ds: &DocuSign,
        envelope: docusign::Envelope,
    ) -> Result<()> {
        // Keep the fields from Airtable we need just in case they changed.
        self.keep_fields_from_airtable(db).await;

        let company = self.company(db)?;

        let send_notification = self.docusign_piia_envelope_status != envelope.status;

        // Set the status in the database and airtable.
        self.docusign_piia_envelope_status = envelope.status.to_string();
        self.piia_envelope_created = envelope.created_date_time;

        // If the document is completed, let's save it to Google Drive.
        if envelope.status != "completed" {
            // We will skip to the end and return early, only updating the status.
            self.update(db).await?;

            if send_notification {
                // Send a slack notification that the docusign status changed.
                self.send_slack_notification_docusign_piia_status_changed(db, &company)
                    .await?;
            }
            return Ok(());
        }

        // Set the completed time.
        self.piia_envelope_completed = envelope.completed_date_time;
        // We do not change the applicant's status or anything since they don't need
        // to complete these docs until their start date.
        // However, other than manually, we should have a gate to make sure they _do_
        // complete these documents before their start date.

        // Let's update the database here since nothing else has to do with that.
        self.update(db).await?;

        if send_notification {
            // Send a slack notification that the docusign status changed.
            self.send_slack_notification_docusign_piia_status_changed(db, &company)
                .await?;
        }

        // Initialize the Google Drive client.
        let drive_client = company.authenticate_google_drive(db).await?;
        // Figure out where our directory is.
        // It should be in the shared drive : "Offer Letters"
        let shared_drive = drive_client.drives().get_by_name("Offer Letters").await?;
        let drive_id = shared_drive.id.to_string();

        // TODO: only save the documents if we don't already have them.
        for document in &envelope.documents {
            let mut bytes = base64::decode(&document.pdf_bytes).unwrap_or_default();
            // Check if we already have bytes to the data.
            if document.pdf_bytes.is_empty() {
                // Get the document from docusign.
                // In order to not "over excessively poll the API here, we need to sleep for 15
                // min before getting each of the documents.
                // https://developers.docusign.com/docs/esign-rest-api/esign101/rules-and-limits/
                //thread::sleep(std::time::Duration::from_secs(15));
                bytes = ds.get_document(&envelope.envelope_id, &document.id).await?.to_vec();
            }

            // Create the folder for our applicant with their name.
            let name_folder_id = drive_client
                .files()
                .create_folder(&shared_drive.id, "", &self.name)
                .await?;

            let mut filename = format!("{} - {}.pdf", self.name, document.name);
            if document.name.contains("Employee Mediation") || document.name.contains("Employee_Mediation") {
                filename = format!("{} - Mediation Agreement.pdf", self.name);
            } else if document.name.contains("Employee Proprietary") || document.name.contains("Employee_Proprietary") {
                filename = format!("{} - PIIA.pdf", self.name);
            } else if document.name.contains("Summary") {
                filename = format!("{} - Employee Agreements - DocuSign Summary.pdf", self.name);
            } else if document.name.contains("Offer Letter") {
                filename = format!("{} - Offer.pdf", self.name);
            }

            // Create or update the file in the google_drive.
            drive_client
                .files()
                .create_or_update(&drive_id, &name_folder_id, &filename, "application/pdf", &bytes)
                .await?;
            info!("uploaded completed file `{}` to drive", filename);
        }

        Ok(())
    }
}

pub async fn refresh_new_applicants_and_reviews(db: &Database, company: &Company) -> Result<()> {
    if company.airtable_base_id_hiring.is_empty() {
        // Return early.
        return Ok(());
    }

    // Initialize the GSuite sheets client.
    let drive_client = company.authenticate_google_drive(db).await?;

    let github = company.authenticate_github()?;

    // Get all the hiring issues on the configs repository.
    let configs_issues = github
        .issues()
        .list_all_for_repo(
            &company.github_org,
            "configs",
            // milestone
            "",
            octorust::types::IssuesListState::All,
            // assignee
            "",
            // creator
            "",
            // mentioned
            "",
            // labels
            "hiring",
            // sort
            Default::default(),
            // direction
            Default::default(),
            // since
            None,
        )
        .await?;

    // We want all the applicants without a sheet id, since this is the list of applicants we care
    // about. Everything else came from Google Sheets and therefore uses the old system.
    let applicants = applicants::dsl::applicants
        .filter(applicants::dsl::sheet_id.eq("".to_string()))
        .load::<Applicant>(&db.conn())?;

    // Iterate over the applicants and update them.
    for mut applicant in applicants {
        applicant.keep_fields_from_airtable(db).await;

        // Expand the application.
        if let Err(e) = applicant.expand(db, &drive_client).await {
            warn!("expanding applicant `{}` failed: {}", applicant.email, e);
            continue;
        }

        // Update the applicant's status based on other criteria.
        applicant.update_status(db, company).await?;

        // Update airtable and the database again, we want to save our status just in
        // case there is an error.
        applicant.update(db).await?;

        // Send the follow up email if we need to, this will also update the database.
        applicant.send_email_follow_up_if_necessary(db).await?;

        // Create the GitHub onboarding issue if we need to.
        applicant
            .create_github_onboarding_issue(db, &github, &configs_issues)
            .await?;

        // Update the interviews start and end time if we have interviews.
        applicant.update_interviews_start_end_time(db);

        // Update airtable and the database again, we want to save our status just in
        // case there is an error.
        applicant.update(db).await?;

        // Update the reviews for the applicant.
        // This function will update the database so we don't have to.
        applicant.update_reviews_scoring(db).await?;

        // TODO: we could move docusign stuff here as well, and out of its own function.
    }

    // Update Airtable.
    // TODO: this might cause some racy problems, maybe only run at night (?)
    // Or maybe always get the latest from the database and update airtable with that (?)
    // Applicants::get_from_db(db, company.id)?.update_airtable(db).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use diesel::prelude::*;
    use serde_json::json;

    use crate::{applicants::Applicant, db::Database, schema::applicants};

    #[test]
    fn test_serialize_deserialize_applicants() {
        crate::utils::setup_logger();

        let db = Database::new();
        let applicant = applicants::dsl::applicants
            .filter(applicants::dsl::id.eq(318))
            .first::<Applicant>(&db.conn())
            .unwrap();

        // Let's test that serializing this is going to give us an array of Airtable users.
        let scorers = json!(applicant).to_string();
        // Let's assert in the string are the scorers formatted as Airtable users.
        assert!(scorers.contains("\"scorers\":[{\"email\":\""));

        // Let's test that deserializing a string will give us the same applicant we had
        // originally.
        let a: Applicant = serde_json::from_str(&scorers).unwrap();
        assert_eq!(applicant, a);
    }
}
