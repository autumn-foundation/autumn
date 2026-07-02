use autumn_web::db::scrub_sql;

fn main() {
    let sql = "SELECT * FROM users WHERE email = 'test@example.com' AND age > 18";
    println!("{}", scrub_sql(sql));
}
