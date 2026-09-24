-- 阶段 5：Web 接入的持久化地基（一次建齐，好友/群组逻辑在阶段 6/7 落地）
--
-- 设计取舍：
-- - 主键统一用雪花 BIGINT：与协议层的 msg_id/session_id 同源，全系统一套
--   ID 空间（避免「DB 自增」与「服务端发号」两套 ID 打架）；
-- - 密码存 argon2 的 PHC 字符串（盐/参数都在字符串里，验签自解释）；
-- - token 是不透明随机串（UUID v4）：可撤销 = 删行。不选 JWT——
--   「服务端能踢下线」比「无状态免查库」对这个项目更重要；
-- - 好友存无向边一行（小 ID 在前）：主键 + CHECK 双保险防重边，
--   查「我的好友」用 OR 两侧索引；
-- - 文件只存元数据：内容落盘 data/files/（数据库不适合当 blob 仓库）。

-- ── 用户 ──────────────────────────────────────────────────────
CREATE TABLE users (
    id            BIGINT PRIMARY KEY,              -- 雪花发号（即协议层 user_id）
    username      TEXT NOT NULL UNIQUE,            -- 登录名（唯一，小写化由应用层保证）
    password_hash TEXT NOT NULL,                   -- argon2 PHC 字符串
    display_name  TEXT NOT NULL,                   -- 昵称（展示用，可重复）
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ── 登录令牌（REST 与 WS 握手共用）──────────────────────────
CREATE TABLE tokens (
    token      TEXT PRIMARY KEY,                   -- UUID v4 不透明串
    user_id    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL                -- 过期即失效（校验时 WHERE 过滤）
);
CREATE INDEX idx_tokens_user ON tokens(user_id);

-- ── 好友请求（状态机：pending → accepted / rejected）────────
CREATE TABLE friend_requests (
    id         BIGINT PRIMARY KEY,                 -- 雪花发号
    from_user  BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    to_user    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    status     TEXT NOT NULL DEFAULT 'pending'
               CHECK (status IN ('pending', 'accepted', 'rejected')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (from_user, to_user)                    -- 同方向重复请求直接撞唯一键
);
CREATE INDEX idx_friend_requests_to ON friend_requests(to_user, status);

-- ── 好友关系（无向边，一行两端）─────────────────────────────
CREATE TABLE friends (
    user_a     BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE, -- min(u1, u2)
    user_b     BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE, -- max(u1, u2)
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_a, user_b),
    CHECK (user_a < user_b)                        -- 无向边的规范化：小 ID 恒在前
);
CREATE INDEX idx_friends_user_a ON friends(user_a);
CREATE INDEX idx_friends_user_b ON friends(user_b);

-- ── 群组 ─────────────────────────────────────────────────────
CREATE TABLE groups (
    id         BIGINT PRIMARY KEY,                 -- 雪花发号（即协议层群消息的 to）
    name       TEXT NOT NULL,
    owner_id   BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ── 群成员（阶段 7 的扇出 actor 从这里取成员快照）──────────
CREATE TABLE group_members (
    group_id   BIGINT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    user_id    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role       TEXT NOT NULL DEFAULT 'member'
               CHECK (role IN ('owner', 'member')),
    joined_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (group_id, user_id)
);
CREATE INDEX idx_group_members_user ON group_members(user_id);

-- ── 文件元数据（内容落盘，DB 只记「谁传的、叫什么、多大」）──
CREATE TABLE files (
    id         BIGINT PRIMARY KEY,                 -- 雪花发号（下载 URL 的 {id}）
    owner_id   BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    filename   TEXT NOT NULL,                      -- 原始文件名（下载时回填）
    size_bytes BIGINT NOT NULL,
    sha256     TEXT NOT NULL,                      -- 完整性校验（传输损坏/篡改检测）
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_files_owner ON files(owner_id);
