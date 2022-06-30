use super::{
    error::*,
    handler::{BindRequest, LoginHandler, UserId},
    opaque_handler::*,
    sql_backend_handler::SqlBackendHandler,
    sql_tables::*,
};
use async_trait::async_trait;
use lldap_auth::opaque;
use sea_query::{Expr, Iden, Query};
use sea_query_binder::SqlxBinder;
use secstr::SecUtf8;
use sqlx::Row;
use tracing::{debug, instrument};

type SqlOpaqueHandler = SqlBackendHandler;

#[instrument(skip_all, level = "debug", err)]
fn passwords_match(
    password_file_bytes: &[u8],
    clear_password: &str,
    server_setup: &opaque::server::ServerSetup,
    username: &UserId,
) -> Result<()> {
    use opaque::{client, server};
    let mut rng = rand::rngs::OsRng;
    let client_login_start_result = client::login::start_login(clear_password, &mut rng)?;

    let password_file = server::ServerRegistration::deserialize(password_file_bytes)
        .map_err(opaque::AuthenticationError::ProtocolError)?;
    let server_login_start_result = server::login::start_login(
        &mut rng,
        server_setup,
        Some(password_file),
        client_login_start_result.message,
        username.as_str(),
    )?;
    client::login::finish_login(
        client_login_start_result.state,
        server_login_start_result.message,
    )?;
    Ok(())
}

impl SqlBackendHandler {
    fn get_orion_secret_key(&self) -> Result<orion::aead::SecretKey> {
        Ok(orion::aead::SecretKey::from_slice(
            self.config.get_server_keys().private(),
        )?)
    }

    #[instrument(skip_all, level = "debug", err)]
    async fn get_password_file_for_user(
        &self,
        username: &str,
    ) -> Result<Option<opaque::server::ServerRegistration>> {
        // Fetch the previously registered password file from the DB.
        let password_file_bytes = {
            let (query, values) = Query::select()
                .column(Users::PasswordHash)
                .from(Users::Table)
                .cond_where(Expr::col(Users::UserId).eq(username))
                .build_sqlx(DbQueryBuilder {});
            if let Some(row) = sqlx::query_with(query.as_str(), values)
                .fetch_optional(&self.sql_pool)
                .await?
            {
                if let Some(bytes) =
                    row.get::<Option<Vec<u8>>, _>(&*Users::PasswordHash.to_string())
                {
                    bytes
                } else {
                    // No password set.
                    return Ok(None);
                }
            } else {
                // No such user.
                return Ok(None);
            }
        };
        opaque::server::ServerRegistration::deserialize(&password_file_bytes)
            .map(Option::Some)
            .map_err(|_| {
                DomainError::InternalError(format!("Corrupted password file for {}", username))
            })
    }
}

#[async_trait]
impl LoginHandler for SqlBackendHandler {
    #[instrument(skip_all, level = "debug", err)]
    async fn bind(&self, request: BindRequest) -> Result<()> {
        let (query, values) = Query::select()
            .column(Users::PasswordHash)
            .from(Users::Table)
            .cond_where(Expr::col(Users::UserId).eq(&request.name))
            .build_sqlx(DbQueryBuilder {});
        if let Ok(row) = sqlx::query_with(&query, values)
            .fetch_one(&self.sql_pool)
            .await
        {
            if let Some(password_hash) =
                row.get::<Option<Vec<u8>>, _>(&*Users::PasswordHash.to_string())
            {
                if let Err(e) = passwords_match(
                    &password_hash,
                    &request.password,
                    self.config.get_server_setup(),
                    &request.name,
                ) {
                    debug!(r#"Invalid password for "{}": {}"#, &request.name, e);
                } else {
                    return Ok(());
                }
            } else {
                debug!(r#"User "{}" has no password"#, &request.name);
            }
        } else {
            debug!(r#"No user found for "{}""#, &request.name);
        }
        Err(DomainError::AuthenticationError(format!(
            " for user '{}'",
            request.name
        )))
    }
}

#[async_trait]
impl OpaqueHandler for SqlOpaqueHandler {
    #[instrument(skip_all, level = "debug", err)]
    async fn login_start(
        &self,
        request: login::ClientLoginStartRequest,
    ) -> Result<login::ServerLoginStartResponse> {
        let maybe_password_file = self.get_password_file_for_user(&request.username).await?;

        let mut rng = rand::rngs::OsRng;
        // Get the CredentialResponse for the user, or a dummy one if no user/no password.
        let start_response = opaque::server::login::start_login(
            &mut rng,
            self.config.get_server_setup(),
            maybe_password_file,
            request.login_start_request,
            &request.username,
        )?;
        let secret_key = self.get_orion_secret_key()?;
        let server_data = login::ServerData {
            username: request.username,
            server_login: start_response.state,
        };
        let encrypted_state = orion::aead::seal(&secret_key, &bincode::serialize(&server_data)?)?;

        Ok(login::ServerLoginStartResponse {
            server_data: base64::encode(&encrypted_state),
            credential_response: start_response.message,
        })
    }

    #[instrument(skip_all, level = "debug", err)]
    async fn login_finish(&self, request: login::ClientLoginFinishRequest) -> Result<UserId> {
        let secret_key = self.get_orion_secret_key()?;
        let login::ServerData {
            username,
            server_login,
        } = bincode::deserialize(&orion::aead::open(
            &secret_key,
            &base64::decode(&request.server_data)?,
        )?)?;
        // Finish the login: this makes sure the client data is correct, and gives a session key we
        // don't need.
        let _session_key =
            opaque::server::login::finish_login(server_login, request.credential_finalization)?
                .session_key;

        Ok(UserId::new(&username))
    }

    #[instrument(skip_all, level = "debug", err)]
    async fn registration_start(
        &self,
        request: registration::ClientRegistrationStartRequest,
    ) -> Result<registration::ServerRegistrationStartResponse> {
        // Generate the server-side key and derive the data to send back.
        let start_response = opaque::server::registration::start_registration(
            self.config.get_server_setup(),
            request.registration_start_request,
            &request.username,
        )?;
        let secret_key = self.get_orion_secret_key()?;
        let server_data = registration::ServerData {
            username: request.username,
        };
        let encrypted_state = orion::aead::seal(&secret_key, &bincode::serialize(&server_data)?)?;
        Ok(registration::ServerRegistrationStartResponse {
            server_data: base64::encode(encrypted_state),
            registration_response: start_response.message,
        })
    }

    #[instrument(skip_all, level = "debug", err)]
    async fn registration_finish(
        &self,
        request: registration::ClientRegistrationFinishRequest,
    ) -> Result<()> {
        let secret_key = self.get_orion_secret_key()?;
        let registration::ServerData { username } = bincode::deserialize(&orion::aead::open(
            &secret_key,
            &base64::decode(&request.server_data)?,
        )?)?;

        let password_file =
            opaque::server::registration::get_password_file(request.registration_upload);
        {
            // Set the user password to the new password.
            let (update_query, values) = Query::update()
                .table(Users::Table)
                .value(Users::PasswordHash, password_file.serialize().into())
                .cond_where(Expr::col(Users::UserId).eq(username))
                .build_sqlx(DbQueryBuilder {});
            sqlx::query_with(update_query.as_str(), values)
                .execute(&self.sql_pool)
                .await?;
        }
        Ok(())
    }
}

/// Convenience function to set a user's password.
#[instrument(skip_all, level = "debug", err)]
pub(crate) async fn register_password(
    opaque_handler: &SqlOpaqueHandler,
    username: &UserId,
    password: &SecUtf8,
) -> Result<()> {
    let mut rng = rand::rngs::OsRng;
    use registration::*;
    let registration_start =
        opaque::client::registration::start_registration(password.unsecure(), &mut rng)?;
    let start_response = opaque_handler
        .registration_start(ClientRegistrationStartRequest {
            username: username.to_string(),
            registration_start_request: registration_start.message,
        })
        .await?;
    let registration_finish = opaque::client::registration::finish_registration(
        registration_start.state,
        start_response.registration_response,
        &mut rng,
    )?;
    opaque_handler
        .registration_finish(ClientRegistrationFinishRequest {
            server_data: start_response.server_data,
            registration_upload: registration_finish.message,
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{
            handler::{BackendHandler, CreateUserRequest},
            sql_backend_handler::SqlBackendHandler,
            sql_tables::init_table,
        },
        infra::configuration::{Configuration, ConfigurationBuilder},
    };

    fn get_default_config() -> Configuration {
        ConfigurationBuilder::default()
            .verbose(true)
            .build()
            .unwrap()
    }

    async fn get_in_memory_db() -> Pool {
        PoolOptions::new().connect("sqlite::memory:").await.unwrap()
    }

    async fn get_initialized_db() -> Pool {
        let sql_pool = get_in_memory_db().await;
        init_table(&sql_pool).await.unwrap();
        sql_pool
    }

    async fn insert_user_no_password(handler: &SqlBackendHandler, name: &str) {
        handler
            .create_user(CreateUserRequest {
                user_id: UserId::new(name),
                email: "bob@bob.bob".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
    }

    async fn attempt_login(
        opaque_handler: &SqlOpaqueHandler,
        username: &str,
        password: &str,
    ) -> Result<()> {
        let mut rng = rand::rngs::OsRng;
        use login::*;
        let login_start = opaque::client::login::start_login(password, &mut rng)?;
        let start_response = opaque_handler
            .login_start(ClientLoginStartRequest {
                username: username.to_string(),
                login_start_request: login_start.message,
            })
            .await?;
        let login_finish = opaque::client::login::finish_login(
            login_start.state,
            start_response.credential_response,
        )?;
        opaque_handler
            .login_finish(ClientLoginFinishRequest {
                server_data: start_response.server_data,
                credential_finalization: login_finish.message,
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_flow() -> Result<()> {
        let sql_pool = get_initialized_db().await;
        let config = get_default_config();
        let backend_handler = SqlBackendHandler::new(config.clone(), sql_pool.clone());
        let opaque_handler = SqlOpaqueHandler::new(config, sql_pool);
        insert_user_no_password(&backend_handler, "bob").await;
        attempt_login(&opaque_handler, "bob", "bob00")
            .await
            .unwrap_err();
        register_password(
            &opaque_handler,
            &UserId::new("bob"),
            &secstr::SecUtf8::from("bob00"),
        )
        .await?;
        attempt_login(&opaque_handler, "bob", "wrong_password")
            .await
            .unwrap_err();
        attempt_login(&opaque_handler, "bob", "bob00").await?;
        Ok(())
    }
}
