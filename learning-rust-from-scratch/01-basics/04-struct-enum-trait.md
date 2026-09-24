# 04 - struct、enum 与 trait

## 本章目标

掌握 Rust 组织数据的三大工具。struct/enum ≈ Java 的类与 record 的合体；
trait 是 Rust 的接口——但比 Java 接口强大得多。

## 一、struct：数据载体

```rust
// 命名字段结构体（最常用）——【Java】record 的可变版
struct User {
    id: u64,
    name: String,           // 拥有所有权
    nickname: Option<String>, // 可选字段用 Option，不用 null！
}

let mut u = User {
    id: 1,
    name: "alice".to_string(),
    nickname: None,
};
u.nickname = Some("艾丽".into());   // 修改需要 u 是 mut

// 更新语法：从旧实例构建新实例（【Java】builder 的 toBuilder()）
let u2 = User { id: 2, ..u };      // 其余字段从 u move/copy 过来
// 注意：name 被 move 走了，之后 u.name 不能再用（除非字段是 Copy）
```

```rust
// 元组结构体：有名字的元组（类型安全！）
struct Meters(f64);
struct Feet(f64);
// fn f(m: Meters) —— 传 Feet 编译不过！【实战】rust-im 的 UserId(u64) 就是这个

// 单元结构体：无数据，只做标记（泛型编程里常见）
struct Marker;
```

### impl：方法与关联函数

```rust
#[derive(Debug, Clone, PartialEq)]   // 派生宏：自动实现常用 trait
struct Rect { width: f64, height: f64 }

impl Rect {
    // 关联函数（无 self）——【Java】static 工厂方法
    fn square(size: f64) -> Self {          // Self 是 impl 类型的别名
        Self { width: size, height: size }
    }

    // 方法：第一个参数 &self（借用，只读）
    fn area(&self) -> f64 { self.width * self.height }

    // &mut self：可变借用
    fn scale(&mut self, k: f64) { self.width *= k; self.height *= k; }

    // self：取得所有权（消费，常见于转换，如 into_inner）
    fn into_tuple(self) -> (f64, f64) { (self.width, self.height) }
}

let mut r = Rect::square(3.0);   // :: 调用关联函数
r.scale(2.0);
println!("{} {}", r.area(), r == Rect::square(6.0));   // . 调用方法
```

> 【Java】方法接收者选择是 Rust 独有且重要：
> `&self` / `&mut self` / `self` 对应「读 / 写 / 拿走」，
> 它就是所有权系统在方法签名上的投影。看到签名就知道这个方法会对对象做什么。

## 二、enum：和类型（sum type）——Java 完全没有的东西

```rust
enum Shape {
    Circle { radius: f64 },
    Rect { w: f64, h: f64 },
    Triangle(f64, f64, f64),   // 元组形态
    Point,                     // 无数据
}

fn area(s: &Shape) -> f64 {
    match s {
        Shape::Circle { radius } => std::f64::consts::PI * radius * radius,
        Shape::Rect { w, h } => w * h,
        Shape::Triangle(a, b, c) => {
            let p = (a + b + c) / 2.0;
            (p * (p - a) * (p - b) * (p - c)).sqrt()   // 海伦公式
        }
        Shape::Point => 0.0,
    }
}
```

**struct 是「与」（每个字段都有）→ 积类型；enum 是「或」（恰好一种）→ 和类型。**

> 【Java】Java 的 enum 每个常量只是单例，不能携带不同的数据形态
> （要模拟得用 sealed interface + record，Java 17+ 的模式）。
> `Option`/`Result` 都是 enum——Rust 的错误处理、空值处理全部建立在和类型上。
> 【实战】rust-im 的协议命令字就是 enum：`Cmd::Handshake | Cmd::Ping | Cmd::Msg { .. }`。

带方法的 enum（状态机的标准形态）：

```rust
enum ConnState { Disconnected, Connecting, Connected { session_id: u64 } }

impl ConnState {
    fn is_online(&self) -> bool {
        matches!(self, ConnState::Connected { .. })   // matches! 宏：只关心匹配与否
    }
}
```

## 三、trait：行为抽象

```rust
trait Area {
    fn area(&self) -> f64;                    // 必须实现

    fn describe(&self) -> String {            // 默认实现（【Java】default 方法）
        format!("面积 {}", self.area())
    }
}

impl Area for Rect {
    fn area(&self) -> f64 { self.width * self.height }
    // describe 用默认实现
}
```

### trait 与 Java 接口的差异

| 能力 | Java interface | Rust trait |
|------|----------------|------------|
| 默认方法 | ✅ default | ✅ |
| 静态方法 | ✅ | ✅（无 self 的关联函数，通过 `Trait::method()` 调） |
| 为外部类型实现接口 | ❌（只能继承） | ✅ `impl Display for ForeignType` |
| 字段 | ❌ | ❌（但可要求关联类型/常量） |
| 泛型约束位置 | `<T extends X>` | `<T: X>` / `impl Trait` |
| 运行时多态 | 天生（对象都有 vtable） | 需显式选择 `dyn Trait`（胖指针） |

**孤儿规则**：trait 或类型至少一个属于当前 crate，才能写 impl。
（防止两个库对同一类型实现同一 trait 打架——Rust 一致性的基石。）

### 静态分发 vs 动态分发

```rust
// 静态分发（泛型）：编译期单态化，零开销，每个具体类型生成一份代码
fn print_area<T: Area>(s: &T) { println!("{}", s.area()); }

// 动态分发（trait 对象）：运行时虚表，胖指针 (data_ptr, vtable)
fn print_area_dyn(s: &dyn Area) { println!("{}", s.area()); }
fn make_box() -> Box<dyn Area> { Box::new(Rect::square(1.0)) }
```

> 【Java】Java 方法调用**全部**是动态分发（虚方法）；
> Rust 默认静态分发（性能更好、可内联），需要异构集合/运行时插件时才用 `dyn`。
> 【实战】rust-im 的弱网模拟策略用 `Box<dyn LossStrategy>`（运行时可替换），
> 协议编解码用泛型（编译期已知，零开销）——两种分发各有归宿。

### 常用标准 trait 速记

```rust
// derive 常客：
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
struct Config { timeout_ms: u64 }

// Debug：{:?} 打印 | Clone：.clone() 深拷贝 | PartialEq：== 比较
// Hash：可做 HashMap 的 key | Default：Default::default() 零值

// 手写 Display（用户可读输出，对应 {}/{} 的 {} 格式）：
use std::fmt;
impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Config({}ms)", self.timeout_ms)
    }
}

// From/Into 转换（写 From 白送 Into）：
impl From<u64> for Config {
    fn from(ms: u64) -> Self { Self { timeout_ms: ms } }
}
let c: Config = 3000.into();
```

### 运算符重载

```rust
use std::ops::Add;

#[derive(Clone, Copy, Debug)]
struct Vec2 { x: f64, y: f64 }

impl Add for Vec2 {                        // 重载 +
    type Output = Vec2;
    fn add(self, rhs: Vec2) -> Vec2 {
        Vec2 { x: self.x + rhs.x, y: self.y + rhs.y }
    }
}
```

> Rust 允许重载的运算符有限（无 `&&`、无 `!` 语义篡改），stdlib 也只用它实现
> 数字/容器运算——没有 C++ 那种 `<<` 当流插入的黑魔法。

## 练习

1. 定义 `enum Expr { Num(f64), Add(Box<Expr>, Box<Expr>), Mul(Box<Expr>, Box<Expr>) }`，
   实现 `fn eval(&Expr) -> f64`。（为什么需要 Box？→ 下一篇所有权）
2. 为上面的 Expr 实现 Display，输出中缀表达式 `(1 + 2) * 3`。
3. 定义 trait `Serializer { fn serialize(&self) -> Vec<u8> }`，为两种类型实现；
   再写 `fn dump(items: &[Box<dyn Serializer>])` 感受动态分发。

## 自测

1. `&self`、`&mut self`、`self` 三种接收者各对应什么语义？
2. struct 与 enum 分别对应「积类型」「和类型」是什么意思？
3. 泛型 `impl Trait` 与 `dyn Trait` 的分发时机与开销差别？各自适合什么场景？
