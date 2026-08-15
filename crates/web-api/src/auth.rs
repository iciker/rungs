//! JWT 签发与验证（使用 jsonwebtoken crate）

use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

/// JWT payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: i32,   // user_id
    pub exp: usize, // 过期时间（Unix timestamp）
}

const TOKEN_TTL_SECS: u64 = 7 * 24 * 3600; // 7 天

/// 生成 JWT
pub fn sign(user_id: i32, secret: &str) -> anyhow::Result<String> {
    let exp = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs()
        + TOKEN_TTL_SECS) as usize;

    let claims = Claims { sub: user_id, exp };
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )?;
    Ok(token)
}

/// 验证 JWT 并返回 Claims
pub fn verify(token: &str, secret: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )?;
    Ok(data.claims)
}
