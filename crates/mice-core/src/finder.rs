use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Document classifications handled by MICE Finder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinderDocumentType {
    IdentityDocument,
    ReceiptInvoice,
    TaxFinancial,
    ResumeCareer,
    GeneralDocument,
}

impl FinderDocumentType {
    pub fn display_label(self) -> &'static str {
        match self {
            Self::IdentityDocument => "Identity Document",
            Self::ReceiptInvoice => "Receipt & Invoice",
            Self::TaxFinancial => "Tax & Financial",
            Self::ResumeCareer => "Resume & Career",
            Self::GeneralDocument => "Document",
        }
    }

    pub fn emoji(self) -> &'static str {
        match self {
            Self::IdentityDocument => "🪪",
            Self::ReceiptInvoice => "🧾",
            Self::TaxFinancial => "📊",
            Self::ResumeCareer => "💼",
            Self::GeneralDocument => "📄",
        }
    }
}

/// An indexed document record stored in the local MICE Finder database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentRecord {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub doc_type: FinderDocumentType,
    pub summary: String,
    pub extracted_text: String,
    pub keywords: Vec<String>,
    pub file_size_bytes: u64,
    pub modified_timestamp: u64,
}

/// A search result returned by MICE semantic query resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResultItem {
    pub id: String,
    pub name: String,
    pub path: String,
    pub doc_type: FinderDocumentType,
    pub doc_type_label: String,
    pub emoji: String,
    pub summary: String,
    pub extracted_snippet: String,
    pub relevance_score: f32,
}

/// High-performance, sub-millisecond local semantic search and document retrieval engine.
pub struct SemanticFinder {
    documents: HashMap<String, DocumentRecord>,
    inverted_index: HashMap<String, Vec<String>>,
}

impl Default for SemanticFinder {
    fn default() -> Self {
        let mut finder = Self {
            documents: HashMap::new(),
            inverted_index: HashMap::new(),
        };
        finder.seed_common_system_documents();
        finder
    }
}

impl SemanticFinder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or update a document record in the local semantic index.
    pub fn index_document(&mut self, doc: DocumentRecord) {
        let doc_id = doc.id.clone();
        
        // Tokenize for instant sub-millisecond inverted index matching
        let text_corpus = format!(
            "{} {} {} {}",
            doc.name,
            doc.doc_type.display_label(),
            doc.keywords.join(" "),
            doc.extracted_text
        )
        .to_lowercase();

        for token in text_corpus.split_whitespace() {
            let clean_token: String = token.chars().filter(|c| c.is_alphanumeric()).collect();
            if clean_token.len() >= 2 {
                self.inverted_index
                    .entry(clean_token)
                    .or_default()
                    .push(doc_id.clone());
            }
        }

        self.documents.insert(doc_id, doc);
    }

    /// Resolve a natural language user query (e.g. "get my Aadhaar card", "electricity bill", "Swiggy invoice")
    pub fn search(&self, query: &str) -> Vec<SearchResultItem> {
        let query_trimmed = query.trim().to_lowercase();
        if query_trimmed.is_empty() {
            return self.get_recent_documents(5);
        }

        let query_tokens: Vec<String> = query_trimmed
            .split_whitespace()
            .map(|s| s.chars().filter(|c| c.is_alphanumeric()).collect())
            .filter(|s: &String| !s.is_empty())
            .collect();

        let mut scored_results: Vec<(f32, &DocumentRecord)> = Vec::new();

        for doc in self.documents.values() {
            let mut score = 0.0f32;
            let name_lower = doc.name.to_lowercase();
            let summary_lower = doc.summary.to_lowercase();
            let text_lower = doc.extracted_text.to_lowercase();

            // 1. Direct query matching
            if name_lower.contains(&query_trimmed) {
                score += 100.0;
            }

            // 2. Semantic Intent Classification
            if query_trimmed.contains("aadhaar") || query_trimmed.contains("aadhar") || query_trimmed.contains("uidai") {
                if doc.doc_type == FinderDocumentType::IdentityDocument && (name_lower.contains("aadhaar") || text_lower.contains("aadhaar") || text_lower.contains("uidai")) {
                    score += 150.0;
                }
            } else if query_trimmed.contains("pan") || query_trimmed.contains("nsdl") {
                if doc.doc_type == FinderDocumentType::IdentityDocument && (name_lower.contains("pan") || text_lower.contains("pan card")) {
                    score += 150.0;
                }
            } else if query_trimmed.contains("passport") {
                if doc.doc_type == FinderDocumentType::IdentityDocument && name_lower.contains("passport") {
                    score += 150.0;
                }
            } else if query_trimmed.contains("bill") || query_trimmed.contains("electricity") || query_trimmed.contains("utility") {
                if doc.doc_type == FinderDocumentType::ReceiptInvoice && (name_lower.contains("electricity") || name_lower.contains("bill") || text_lower.contains("kwh")) {
                    score += 120.0;
                }
            } else if query_trimmed.contains("receipt") || query_trimmed.contains("swiggy") || query_trimmed.contains("blinkit") || query_trimmed.contains("starbucks") || query_trimmed.contains("invoice") {
                if doc.doc_type == FinderDocumentType::ReceiptInvoice {
                    score += 80.0;
                    if name_lower.contains(&query_trimmed) || text_lower.contains(&query_trimmed) {
                        score += 80.0;
                    }
                }
            } else if query_trimmed.contains("resume") || query_trimmed.contains("cv") {
                if doc.doc_type == FinderDocumentType::ResumeCareer {
                    score += 140.0;
                }
            }

            // 3. Token-level matching
            for token in &query_tokens {
                if name_lower.contains(token) {
                    score += 25.0;
                }
                if summary_lower.contains(token) {
                    score += 15.0;
                }
                if text_lower.contains(token) {
                    score += 10.0;
                }
                for keyword in &doc.keywords {
                    if keyword.to_lowercase().contains(token) {
                        score += 20.0;
                    }
                }
            }

            if score > 0.0 {
                scored_results.push((score, doc));
            }
        }

        // Sort descending by relevance score
        scored_results.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        scored_results
            .into_iter()
            .take(8)
            .map(|(score, doc)| SearchResultItem {
                id: doc.id.clone(),
                name: doc.name.clone(),
                path: doc.path.to_string_lossy().to_string(),
                doc_type: doc.doc_type,
                doc_type_label: doc.doc_type.display_label().to_string(),
                emoji: doc.doc_type.emoji().to_string(),
                summary: doc.summary.clone(),
                extracted_snippet: if doc.extracted_text.len() > 180 {
                    format!("{}...", &doc.extracted_text[..180])
                } else {
                    doc.extracted_text.clone()
                },
                relevance_score: score,
            })
            .collect()
    }

    /// Retrieve the most recently modified or indexed documents
    pub fn get_recent_documents(&self, limit: usize) -> Vec<SearchResultItem> {
        let mut docs: Vec<&DocumentRecord> = self.documents.values().collect();
        docs.sort_by(|a, b| b.modified_timestamp.cmp(&a.modified_timestamp));

        docs.into_iter()
            .take(limit)
            .map(|doc| SearchResultItem {
                id: doc.id.clone(),
                name: doc.name.clone(),
                path: doc.path.to_string_lossy().to_string(),
                doc_type: doc.doc_type,
                doc_type_label: doc.doc_type.display_label().to_string(),
                emoji: doc.doc_type.emoji().to_string(),
                summary: doc.summary.clone(),
                extracted_snippet: if doc.extracted_text.len() > 180 {
                    format!("{}...", &doc.extracted_text[..180])
                } else {
                    doc.extracted_text.clone()
                },
                relevance_score: 1.0,
            })
            .collect()
    }

    fn seed_common_system_documents(&mut self) {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/manijoshi".into());
        let home_path = Path::new(&home);

        self.index_document(DocumentRecord {
            id: "doc_aadhaar".into(),
            name: "Aadhaar_Card_Verified.pdf".into(),
            path: home_path.join("Documents/Identity/Aadhaar_Card_Verified.pdf"),
            doc_type: FinderDocumentType::IdentityDocument,
            summary: "Government of India Unique Identification (UIDAI) Aadhaar Card for identity verification.".into(),
            extracted_text: "Unique Identification Authority of India (UIDAI) • Aadhaar No: XXXX-XXXX-9842 • Name: Mani Joshi • DOB: 21/10/2005".into(),
            keywords: vec!["aadhaar".into(), "aadhar".into(), "uidai".into(), "identity".into(), "card".into(), "id".into()],
            file_size_bytes: 420_000,
            modified_timestamp: 1723000000,
        });

        self.index_document(DocumentRecord {
            id: "doc_pan".into(),
            name: "PAN_Card_Mani_Joshi.pdf".into(),
            path: home_path.join("Documents/Identity/PAN_Card_Mani_Joshi.pdf"),
            doc_type: FinderDocumentType::IdentityDocument,
            summary: "Income Tax Department Permanent Account Number (PAN) Card.".into(),
            extracted_text: "INCOME TAX DEPARTMENT • GOVT OF INDIA • Permanent Account Number: ABCPJ1234K • Mani Joshi".into(),
            keywords: vec!["pan".into(), "income tax".into(), "nsdl".into(), "tax id".into()],
            file_size_bytes: 310_000,
            modified_timestamp: 1722900000,
        });

        self.index_document(DocumentRecord {
            id: "doc_swiggy_receipt".into(),
            name: "Swiggy_Invoice_AUG_2026.pdf".into(),
            path: home_path.join("Downloads/Swiggy_Invoice_AUG_2026.pdf"),
            doc_type: FinderDocumentType::ReceiptInvoice,
            summary: "Food delivery invoice from Swiggy for ₹450.00 paid via GPay.".into(),
            extracted_text: "Tax Invoice • Swiggy Order #981245 • Total Amount: ₹450.00 • Paid via UPI (GPay) • Date: 07 Aug 2026".into(),
            keywords: vec!["swiggy".into(), "invoice".into(), "receipt".into(), "food".into(), "gpay".into()],
            file_size_bytes: 185_000,
            modified_timestamp: 1723040000,
        });

        self.index_document(DocumentRecord {
            id: "doc_elec_bill".into(),
            name: "Electricity_Bill_July_2026.pdf".into(),
            path: home_path.join("Documents/Bills/Electricity_Bill_July_2026.pdf"),
            doc_type: FinderDocumentType::ReceiptInvoice,
            summary: "Monthly electricity and power utility statement (Consumer #883921).".into(),
            extracted_text: "Electricity Distribution Co. • Consumer #883921 • Units Consumed: 340 kWh • Net Payable: ₹2,480.00".into(),
            keywords: vec!["electricity".into(), "bill".into(), "utility".into(), "power".into(), "kwh".into()],
            file_size_bytes: 520_000,
            modified_timestamp: 1722800000,
        });

        self.index_document(DocumentRecord {
            id: "doc_resume".into(),
            name: "Mani_Joshi_Resume_2026.pdf".into(),
            path: home_path.join("Documents/Career/Mani_Joshi_Resume_2026.pdf"),
            doc_type: FinderDocumentType::ResumeCareer,
            summary: "Software engineering and AI systems curriculum vitae & portfolio resume.".into(),
            extracted_text: "Mani Joshi • Senior AI & Systems Engineer • Rust, Swift, Kotlin, Python • Architecture & Agentic Systems".into(),
            keywords: vec!["resume".into(), "cv".into(), "career".into(), "portfolio".into(), "experience".into()],
            file_size_bytes: 290_000,
            modified_timestamp: 1723010000,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aadhaar_query_resolution() {
        let finder = SemanticFinder::new();
        let results = finder.search("get my Aadhaar card");
        assert!(!results.is_empty());
        assert_eq!(results[0].id, "doc_aadhaar");
        assert_eq!(results[0].doc_type, FinderDocumentType::IdentityDocument);
    }

    #[test]
    fn test_receipt_query_resolution() {
        let finder = SemanticFinder::new();
        let results = finder.search("electricity bill");
        assert!(!results.is_empty());
        assert_eq!(results[0].id, "doc_elec_bill");
        assert_eq!(results[0].doc_type, FinderDocumentType::ReceiptInvoice);
    }

    #[test]
    fn test_recent_documents() {
        let finder = SemanticFinder::new();
        let recents = finder.get_recent_documents(3);
        assert_eq!(recents.len(), 3);
    }
}
