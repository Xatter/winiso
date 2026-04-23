pub struct Edition {
    pub id: &'static str,
    pub name: &'static str,
    pub arch: &'static str,
}

pub struct Product {
    pub key: &'static str,
    pub name: &'static str,
    pub editions: &'static [Edition],
    pub segment: &'static str,
}

pub static PRODUCTS: &[Product] = &[
    Product {
        key: "windows11",
        name: "Windows 11",
        editions: &[
            Edition { id: "3321", name: "Windows 11 (x64)", arch: "x64" },
            Edition { id: "3324", name: "Windows 11 (ARM64)", arch: "ARM64" },
        ],
        segment: "windows11",
    },
    Product {
        key: "windows10",
        name: "Windows 10",
        editions: &[
            Edition { id: "2618", name: "Windows 10 (x64)", arch: "x64" },
        ],
        segment: "windows10ISO",
    },
];

pub fn get_product(key: &str) -> Option<&'static Product> {
    PRODUCTS.iter().find(|p| p.key == key)
}

pub fn get_edition<'a>(product: &'a Product, arch: &str) -> &'a Edition {
    product.editions.iter()
        .find(|e| e.arch.eq_ignore_ascii_case(arch))
        .unwrap_or(&product.editions[0])
}
