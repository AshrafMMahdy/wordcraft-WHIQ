//! WHIQ round-trip corpus: hand-written Word packages (written for these tests from ECMA-376 and
//! the published [MS-DOCX] extensions; no file produced by Word) covering what a customer's
//! document carries beyond plain text: tracked changes and moves, comments, SmartArt, a chart
//! with its workbook, custom XML (block, inline, data binding), `w14` markup, markup in an
//! ignorable future namespace, `mc:AlternateContent`, an OLE object, a 3-D model, headers and
//! footers with fields, footnotes, and a macro project.
//!
//! [`corpus`] returns `(file name, package bytes)` pairs; `corpus_roundtrip.rs` opens and saves
//! each one and checks with `wordcraft_docx::roundtrip` that nothing was dropped.
#![allow(dead_code)]

use std::io::Write;

pub const NS: &str = concat!(
    r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" "#,
    r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" "#,
    r#"xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" "#,
    r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" "#,
    r#"xmlns:pic="http://schemas.openxmlformats.org/drawingml/2006/picture" "#,
    r#"xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" "#,
    r#"xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" "#,
    r#"xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" "#,
    r#"xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml" "#,
    r#"xmlns:w15="http://schemas.microsoft.com/office/word/2012/wordml" "#,
    r#"xmlns:w16du="http://schemas.microsoft.com/office/word/2023/wordml/word16du" "#,
    r#"xmlns:wp14="http://schemas.microsoft.com/office/word/2010/wordprocessingDrawing" "#,
    r#"xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape" "#,
    r#"xmlns:am3d="http://schemas.microsoft.com/office/drawing/2017/model3d" "#,
    r#"xmlns:v="urn:schemas-microsoft-com:vml" "#,
    r#"xmlns:o="urn:schemas-microsoft-com:office:office" "#,
    r#"xmlns:w10="urn:schemas-microsoft-com:office:word" "#,
    r#"xmlns:fut="urn:example:future-markup" "#,
    r#"mc:Ignorable="w14 w15 w16du wp14 fut""#
);

const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const DATE: &str = "2026-01-02T03:04:05Z";
/// A 1x1 PNG.
pub const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
    0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x89, 0x99, 0x3d, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

const SECT: &str = r#"<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr>"#;

pub fn zip(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        zw.start_file(name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
        zw.write_all(bytes).unwrap();
    }
    zw.finish().unwrap().into_inner()
}

fn rels(list: &[(&str, &str, &str)]) -> Vec<u8> {
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    for (id, ty, target) in list {
        let ty = if ty.starts_with("http") { (*ty).to_string() } else { format!("{REL}/{ty}") };
        s.push_str(&format!(r#"<Relationship Id="{id}" Type="{ty}" Target="{target}"/>"#));
    }
    s.push_str("</Relationships>");
    s.into_bytes()
}

/// Content types: the defaults every package here uses, plus `overrides` (part, type).
fn content_types(main: &str, overrides: &[(&str, &str)]) -> Vec<u8> {
    let mut s = String::from(concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#,
        r#"<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>"#,
        r#"<Default Extension="xml" ContentType="application/xml"/>"#,
        r#"<Default Extension="png" ContentType="image/png"/>"#,
        r#"<Default Extension="bin" ContentType="application/vnd.openxmlformats-officedocument.oleObject"/>"#,
        r#"<Default Extension="glb" ContentType="model/gltf.binary"/>"#,
        r#"<Default Extension="xlsx" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"/>"#,
    ));
    s.push_str(&format!(r#"<Override PartName="/word/document.xml" ContentType="{main}"/>"#));
    s.push_str(r#"<Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/>"#);
    for (part, ty) in overrides {
        s.push_str(&format!(r#"<Override PartName="/{part}" ContentType="{ty}"/>"#));
    }
    s.push_str("</Types>");
    s.into_bytes()
}

const MAIN: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";

const STYLES: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
    r#"<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style>"#,
    r#"<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:pPr><w:outlineLvl w:val="0"/></w:pPr><w:rPr><w:b/><w:sz w:val="32"/></w:rPr></w:style>"#,
    r#"</w:styles>"#
);

/// A package: `body`, the main document's relationships (styles added), and extra parts.
fn package(main: &str, body: &str, doc_rels: &[(&str, &str, &str)], overrides: &[(&str, &str)], extra: Vec<(String, Vec<u8>)>) -> Vec<u8> {
    let doc = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document {NS}><w:body>{body}{SECT}</w:body></w:document>"#);
    let mut drels = vec![("rIdStyles", "styles", "styles.xml")];
    drels.extend_from_slice(doc_rels);
    let mut entries = vec![
        ("[Content_Types].xml".to_string(), content_types(main, overrides)),
        ("_rels/.rels".to_string(), rels(&[("rId1", "officeDocument", "word/document.xml")])),
        ("word/document.xml".to_string(), doc.into_bytes()),
        ("word/_rels/document.xml.rels".to_string(), rels(&drels)),
        ("word/styles.xml".to_string(), STYLES.as_bytes().to_vec()),
    ];
    entries.extend(extra);
    zip(&entries)
}

fn s(x: &str) -> Vec<u8> {
    x.as_bytes().to_vec()
}

/// Tracked insertions, deletions, formatting changes, a tracked paragraph mark, a move and a
/// comment.
pub fn tracked() -> Vec<u8> {
    let body = format!(
        concat!(
            r#"<w:p w14:paraId="1A2B3C4D" w14:textId="77777777" w:rsidR="00A1B2C3"><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Review copy</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t xml:space="preserve">Keep </w:t></w:r><w:ins w:id="1" w:author="Ada" w:date="{d}" w16du:dateUtc="{d}"><w:r><w:t>added</w:t></w:r></w:ins>"#,
            r#"<w:del w:id="2" w:author="Ben" w:date="{d}"><w:r><w:delText xml:space="preserve"> removed</w:delText></w:r></w:del>"#,
            r#"<w:r><w:rPr><w:b/><w:rPrChange w:id="3" w:author="Ada" w:date="{d}"><w:rPr/></w:rPrChange></w:rPr><w:t xml:space="preserve"> bolded</w:t></w:r></w:p>"#,
            r#"<w:p><w:pPr><w:jc w:val="center"/><w:pPrChange w:id="4" w:author="Ben" w:date="{d}"><w:pPr/></w:pPrChange></w:pPr><w:r><w:t>Centred later</w:t></w:r></w:p>"#,
            r#"<w:p><w:moveFromRangeStart w:id="5" w:author="Ada" w:date="{d}" w:name="move1"/><w:moveFrom w:id="6" w:author="Ada" w:date="{d}"><w:r><w:t>Moved sentence.</w:t></w:r></w:moveFrom><w:moveFromRangeEnd w:id="5"/><w:r><w:t xml:space="preserve"> Stays.</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Middle.</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t xml:space="preserve">Arrives: </w:t></w:r><w:moveToRangeStart w:id="7" w:author="Ada" w:date="{d}" w:name="move1"/><w:moveTo w:id="8" w:author="Ada" w:date="{d}"><w:r><w:t>Moved sentence.</w:t></w:r></w:moveTo><w:moveToRangeEnd w:id="7"/></w:p>"#,
            r#"<w:p><w:pPr><w:rPr><w:ins w:id="9" w:author="Ada" w:date="{d}"/></w:rPr></w:pPr><w:commentRangeStart w:id="0"/><w:r><w:t>Commented text</w:t></w:r><w:commentRangeEnd w:id="0"/><w:r><w:commentReference w:id="0"/></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Last paragraph.</w:t></w:r></w:p>"#,
        ),
        d = DATE
    );
    let comments = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:comments {NS}><w:comment w:id="0" w:author="Ben" w:date="{DATE}" w:initials="B"><w:p><w:r><w:t>Please check.</w:t></w:r></w:p></w:comment></w:comments>"#
    );
    package(
        MAIN,
        &body,
        &[("rIdC", "comments", "comments.xml")],
        &[("word/comments.xml", "application/vnd.openxmlformats-officedocument.wordprocessingml.comments+xml")],
        vec![("word/comments.xml".into(), comments.into_bytes())],
    )
}

fn inline_graphic(name: &str, uri: &str, inner: &str) -> String {
    format!(
        r#"<w:drawing><wp:inline distT="0" distB="0" distL="0" distR="0"><wp:extent cx="2743200" cy="1828800"/><wp:effectExtent l="0" t="0" r="0" b="0"/><wp:docPr id="1" name="{name}"/><wp:cNvGraphicFramePr/><a:graphic><a:graphicData uri="{uri}">{inner}</a:graphicData></a:graphic></wp:inline></w:drawing>"#
    )
}

/// A SmartArt diagram: the drawing and its five diagram parts.
pub fn smart_art() -> Vec<u8> {
    let dgm = inline_graphic(
        "Diagram 1",
        "http://schemas.openxmlformats.org/drawingml/2006/diagram",
        r#"<dgm:relIds r:dm="rIdDm" r:lo="rIdLo" r:qs="rIdQs" r:cs="rIdCs"/>"#,
    );
    let body = format!(
        r#"<w:p><w:r><w:t>Before the diagram.</w:t></w:r></w:p><w:p><w:r>{dgm}</w:r></w:p><w:p><w:r><w:t>After the diagram.</w:t></w:r></w:p>"#
    );
    let dgm_ns = r#"xmlns:dgm="http://schemas.openxmlformats.org/drawingml/2006/diagram" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
    let data = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><dgm:dataModel {dgm_ns}><dgm:ptLst><dgm:pt modelId="{{00000000-0000-0000-0000-000000000001}}" type="doc"><dgm:prSet loTypeId="urn:microsoft.com/office/officeart/2005/8/layout/default"/><dgm:spPr/><dgm:t><a:bodyPr/><a:p><a:endParaRPr lang="en-US"/></a:p></dgm:t></dgm:pt><dgm:pt modelId="{{00000000-0000-0000-0000-000000000002}}"><dgm:prSet/><dgm:spPr/><dgm:t><a:bodyPr/><a:p><a:r><a:rPr lang="en-US"/><a:t>Plan</a:t></a:r></a:p></dgm:t></dgm:pt><dgm:pt modelId="{{00000000-0000-0000-0000-000000000003}}"><dgm:prSet/><dgm:spPr/><dgm:t><a:bodyPr/><a:p><a:r><a:rPr lang="en-US"/><a:t>Build</a:t></a:r></a:p></dgm:t></dgm:pt></dgm:ptLst><dgm:cxnLst><dgm:cxn modelId="{{00000000-0000-0000-0000-000000000010}}" srcId="{{00000000-0000-0000-0000-000000000001}}" destId="{{00000000-0000-0000-0000-000000000002}}" srcOrd="0" destOrd="0"/><dgm:cxn modelId="{{00000000-0000-0000-0000-000000000011}}" srcId="{{00000000-0000-0000-0000-000000000001}}" destId="{{00000000-0000-0000-0000-000000000003}}" srcOrd="1" destOrd="0"/></dgm:cxnLst><dgm:bg/><dgm:whole/><dgm:extLst><a:ext uri="http://schemas.microsoft.com/office/drawing/2008/diagram"><dsp:dataModelExt xmlns:dsp="http://schemas.microsoft.com/office/drawing/2008/diagram" relId="rIdDd" minVer="http://schemas.openxmlformats.org/drawingml/2006/diagram"/></a:ext></dgm:extLst></dgm:dataModel>"#
    );
    let layout = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><dgm:layoutDef {dgm_ns} uniqueId="urn:microsoft.com/office/officeart/2005/8/layout/default"><dgm:title val=""/><dgm:desc val=""/><dgm:layoutNode name="diagram"><dgm:alg type="snake"/></dgm:layoutNode></dgm:layoutDef>"#
    );
    let qs = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><dgm:styleDef {dgm_ns} uniqueId="urn:microsoft.com/office/officeart/2005/8/quickstyle/simple1"><dgm:title val=""/><dgm:desc val=""/><dgm:scene3d><a:camera prst="orthographicFront"/><a:lightRig rig="threePt" dir="t"/></dgm:scene3d></dgm:styleDef>"#
    );
    let cs = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><dgm:colorsDef {dgm_ns} uniqueId="urn:microsoft.com/office/officeart/2005/8/colors/accent1_2"><dgm:title val=""/><dgm:desc val=""/></dgm:colorsDef>"#
    );
    let drawing = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><dsp:drawing xmlns:dsp="http://schemas.microsoft.com/office/drawing/2008/diagram" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><dsp:spTree><dsp:nvGrpSpPr><dsp:cNvPr id="0" name=""/><dsp:cNvGrpSpPr/></dsp:nvGrpSpPr><dsp:grpSpPr/></dsp:spTree></dsp:drawing>"#;
    package(
        MAIN,
        &body,
        &[
            ("rIdDm", "diagramData", "diagrams/data1.xml"),
            ("rIdLo", "diagramLayout", "diagrams/layout1.xml"),
            ("rIdQs", "diagramQuickStyle", "diagrams/quickStyle1.xml"),
            ("rIdCs", "diagramColors", "diagrams/colors1.xml"),
            ("rIdDd", "http://schemas.microsoft.com/office/2007/relationships/diagramDrawing", "diagrams/drawing1.xml"),
        ],
        &[
            ("word/diagrams/data1.xml", "application/vnd.openxmlformats-officedocument.drawingml.diagramData+xml"),
            ("word/diagrams/layout1.xml", "application/vnd.openxmlformats-officedocument.drawingml.diagramLayout+xml"),
            ("word/diagrams/quickStyle1.xml", "application/vnd.openxmlformats-officedocument.drawingml.diagramStyle+xml"),
            ("word/diagrams/colors1.xml", "application/vnd.openxmlformats-officedocument.drawingml.diagramColors+xml"),
            ("word/diagrams/drawing1.xml", "application/vnd.ms-office.drawingml.diagramDrawing+xml"),
        ],
        vec![
            ("word/diagrams/data1.xml".into(), data.into_bytes()),
            ("word/diagrams/layout1.xml".into(), layout.into_bytes()),
            ("word/diagrams/quickStyle1.xml".into(), qs.into_bytes()),
            ("word/diagrams/colors1.xml".into(), cs.into_bytes()),
            ("word/diagrams/drawing1.xml".into(), s(drawing)),
        ],
    )
}

/// A bar chart with cached values and its embedded workbook.
pub fn chart() -> Vec<u8> {
    let g = inline_graphic("Chart 1", "http://schemas.openxmlformats.org/drawingml/2006/chart", r#"<c:chart r:id="rIdChart"/>"#);
    let body = format!(r#"<w:p><w:r><w:t>Sales by quarter:</w:t></w:r></w:p><w:p><w:r>{g}</w:r></w:p>"#);
    let chart = concat!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
        r#"<c:chart><c:autoTitleDeleted val="1"/><c:plotArea><c:layout/><c:barChart><c:barDir val="col"/><c:grouping val="clustered"/><c:varyColors val="0"/>"#,
        r#"<c:ser><c:idx val="0"/><c:order val="0"/><c:tx><c:strRef><c:f>Sheet1!$B$1</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>Sales</c:v></c:pt></c:strCache></c:strRef></c:tx>"#,
        r#"<c:cat><c:strRef><c:f>Sheet1!$A$2:$A$3</c:f><c:strCache><c:ptCount val="2"/><c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt></c:strCache></c:strRef></c:cat>"#,
        r#"<c:val><c:numRef><c:f>Sheet1!$B$2:$B$3</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val="2"/><c:pt idx="0"><c:v>4</c:v></c:pt><c:pt idx="1"><c:v>7</c:v></c:pt></c:numCache></c:numRef></c:val></c:ser>"#,
        r#"<c:axId val="1"/><c:axId val="2"/></c:barChart><c:catAx><c:axId val="1"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/><c:crossAx val="2"/></c:catAx>"#,
        r#"<c:valAx><c:axId val="2"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="l"/><c:crossAx val="1"/></c:valAx></c:plotArea><c:plotVisOnly val="1"/></c:chart>"#,
        r#"<c:externalData r:id="rIdWb"><c:autoUpdate val="0"/></c:externalData></c:chartSpace>"#
    );
    package(
        MAIN,
        &body,
        &[("rIdChart", "chart", "charts/chart1.xml")],
        &[("word/charts/chart1.xml", "application/vnd.openxmlformats-officedocument.drawingml.chart+xml")],
        vec![
            ("word/charts/chart1.xml".into(), s(chart)),
            ("word/charts/_rels/chart1.xml.rels".into(), rels(&[("rIdWb", "package", "../embeddings/Microsoft_Excel_Worksheet1.xlsx")])),
            ("word/embeddings/Microsoft_Excel_Worksheet1.xlsx".into(), b"PK\x05\x06not-a-real-workbook-but-carried-verbatim".to_vec()),
        ],
    )
}

/// Custom XML: a block-level element around a paragraph, an inline one around a run, a content
/// control bound to a custom XML part, and the part itself.
pub fn custom_xml() -> Vec<u8> {
    let body = concat!(
        r#"<w:customXml w:uri="urn:example:contract" w:element="clause"><w:customXmlPr><w:attr w:name="id" w:val="c1"/></w:customXmlPr><w:p><w:r><w:t>Clause text.</w:t></w:r></w:p></w:customXml>"#,
        r#"<w:p><w:r><w:t xml:space="preserve">Party: </w:t></w:r><w:customXml w:uri="urn:example:contract" w:element="party"><w:r><w:t>ACME Ltd</w:t></w:r></w:customXml></w:p>"#,
        r#"<w:sdt><w:sdtPr><w:alias w:val="Title"/><w:tag w:val="title"/><w:id w:val="123"/><w:dataBinding w:prefixMappings="xmlns:ns0='urn:example:props'" w:xpath="/ns0:root[1]/ns0:title[1]" w:storeItemID="{11111111-2222-3333-4444-555555555555}"/><w:text/></w:sdtPr><w:sdtContent><w:p><w:r><w:t>Bound title</w:t></w:r></w:p></w:sdtContent></w:sdt>"#,
        r#"<w:p><w:r><w:t>Closing paragraph.</w:t></w:r></w:p>"#
    );
    let item = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><root xmlns="urn:example:props"><title>Bound title</title></root>"#;
    let props = r#"<?xml version="1.0" encoding="UTF-8" standalone="no"?><ds:datastoreItem ds:itemID="{11111111-2222-3333-4444-555555555555}" xmlns:ds="http://schemas.openxmlformats.org/officeDocument/2006/customXml"><ds:schemaRefs><ds:schemaRef ds:uri="urn:example:props"/></ds:schemaRefs></ds:datastoreItem>"#;
    package(
        MAIN,
        body,
        &[("rIdX", "customXml", "../customXml/item1.xml")],
        &[("customXml/itemProps1.xml", "application/vnd.openxmlformats-officedocument.customXmlProperties+xml")],
        vec![
            ("customXml/item1.xml".into(), s(item)),
            ("customXml/itemProps1.xml".into(), s(props)),
            ("customXml/_rels/item1.xml.rels".into(), rels(&[("rId1", "customXmlProps", "itemProps1.xml")])),
        ],
    )
}

/// `w14` run effects, a `w14` check box, a shape in `mc:AlternateContent` with a VML fallback,
/// bidirectional overrides, proofing marks, and elements in an ignorable namespace no reader
/// knows (block level, inline, in a run).
pub fn w14_markup() -> Vec<u8> {
    let body = concat!(
        r#"<w:p w14:paraId="0A0B0C0D" w14:textId="0E0F1011"><w:r><w:rPr><w14:textOutline w14:w="9525" w14:cap="flat" w14:cmpd="sng" w14:algn="ctr"><w14:solidFill><w14:srgbClr w14:val="FF0000"/></w14:solidFill><w14:prstDash w14:val="solid"/><w14:round/></w14:textOutline><w14:ligatures w14:val="standard"/></w:rPr><w:t>Outlined text</w:t></w:r></w:p>"#,
        r#"<w:p><w:proofErr w:type="spellStart"/><w:r><w:t>Speling</w:t></w:r><w:proofErr w:type="spellEnd"/><w:r><w:t xml:space="preserve"> and </w:t></w:r><w:bdo w:val="rtl"><w:r><w:t>reversed</w:t></w:r></w:bdo><w:r><w:t xml:space="preserve"> </w:t></w:r><w:dir w:val="rtl"><w:r><w:t>embedded</w:t></w:r></w:dir></w:p>"#,
        r#"<w:sdt><w:sdtPr><w:id w:val="99"/><w14:checkbox><w14:checked w14:val="1"/><w14:checkedState w14:val="2612" w14:font="MS Gothic"/><w14:uncheckedState w14:val="2610" w14:font="MS Gothic"/></w14:checkbox></w:sdtPr><w:sdtContent><w:p><w:r><w:t>☒</w:t></w:r><w:r><w:t xml:space="preserve"> Agreed</w:t></w:r></w:p></w:sdtContent></w:sdt>"#,
        r#"<fut:sectionNote fut:kind="block"><fut:detail>future block-level markup</fut:detail></fut:sectionNote>"#,
        r#"<w:p><w:r><w:t xml:space="preserve">Inline </w:t></w:r><fut:marker fut:id="m1"/><w:r><w:t>future</w:t></w:r><w:r><fut:runThing fut:v="2"/><w:t xml:space="preserve"> markup</w:t></w:r></w:p>"#,
        r##"<w:p><w:r><mc:AlternateContent><mc:Choice Requires="wps"><w:drawing><wp:anchor distT="0" distB="0" distL="114300" distR="114300" simplePos="0" relativeHeight="251659264" behindDoc="0" locked="0" layoutInCell="1" allowOverlap="1"><wp:simplePos x="0" y="0"/><wp:positionH relativeFrom="column"><wp:posOffset>0</wp:posOffset></wp:positionH><wp:positionV relativeFrom="paragraph"><wp:posOffset>0</wp:posOffset></wp:positionV><wp:extent cx="914400" cy="457200"/><wp:effectExtent l="0" t="0" r="0" b="0"/><wp:wrapSquare wrapText="bothSides"/><wp:docPr id="5" name="Rectangle 5"/><wp:cNvGraphicFramePr/><a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/word/2010/wordprocessingShape"><wps:wsp><wps:cNvSpPr/><wps:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="457200"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom><a:solidFill><a:srgbClr val="4472C4"/></a:solidFill></wps:spPr><wps:bodyPr/></wps:wsp></a:graphicData></a:graphic></wp:anchor></w:drawing></mc:Choice><mc:Fallback><w:pict><v:rect id="Rectangle 5" o:spid="_x0000_s1026" style="position:absolute;width:72pt;height:36pt" fillcolor="#4472c4"><w10:wrap type="square"/></v:rect></w:pict></mc:Fallback></mc:AlternateContent></w:r><w:r><w:t>Shape anchor paragraph.</w:t></w:r></w:p>"##,
        r#"<w:p><w:r><w:t>End.</w:t></w:r></w:p>"#
    );
    package(MAIN, body, &[], &[], vec![])
}

/// An embedded OLE object (a package object shown as an icon) with its preview picture.
pub fn ole() -> Vec<u8> {
    let body = concat!(
        r##"<w:p><w:r><w:t xml:space="preserve">See the attachment: </w:t></w:r><w:r><w:object w:dxaOrig="1440" w:dyaOrig="1440"><v:shape id="_x0000_i1025" type="#_x0000_t75" style="width:72pt;height:72pt" o:ole=""><v:imagedata r:id="rIdImg" o:title=""/></v:shape><o:OLEObject Type="Embed" ProgID="Package" ShapeID="_x0000_i1025" DrawAspect="Icon" ObjectID="_1234567890" r:id="rIdOle"/></w:object></w:r></w:p>"##,
        r#"<w:p><w:r><w:t>After the object.</w:t></w:r></w:p>"#
    );
    package(
        MAIN,
        body,
        &[("rIdImg", "image", "media/image1.png"), ("rIdOle", "oleObject", "embeddings/oleObject1.bin")],
        &[],
        vec![("word/media/image1.png".into(), PNG.to_vec()), ("word/embeddings/oleObject1.bin".into(), b"\xd0\xcf\x11\xe0opaque ole bytes".to_vec())],
    )
}

/// A 3-D model in `mc:AlternateContent` (the `am3d` choice, a picture fallback).
pub fn model_3d() -> Vec<u8> {
    let choice = inline_graphic(
        "3D Model 1",
        "http://schemas.microsoft.com/office/drawing/2017/model3d",
        r#"<am3d:model3d r:embed="rIdGlb"><am3d:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="2743200" cy="1828800"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></am3d:spPr><am3d:camera><am3d:pos x="0" y="0" z="70000000"/><am3d:up dx="0" dy="36000000" dz="0"/><am3d:lookAt x="0" y="0" z="0"/><am3d:perspective fov="2700000"/></am3d:camera><am3d:trans><am3d:meterPerModelUnit n="1" d="1"/><am3d:preTrans dx="0" dy="0" dz="0"/><am3d:scale><am3d:sx n="1" d="1"/><am3d:sy n="1" d="1"/><am3d:sz n="1" d="1"/></am3d:scale><am3d:rot/><am3d:postTrans dx="0" dy="0" dz="0"/></am3d:trans><am3d:raster rName="Office3DRenderer" rVer="16.0.8326"><am3d:blip r:embed="rIdPng"/></am3d:raster><am3d:objViewport viewportSz="2743200"/><am3d:ambientLight><am3d:clr><a:scrgbClr r="50000" g="50000" b="50000"/></am3d:clr><am3d:illuminance n="500000" d="1000000"/></am3d:ambientLight></am3d:model3d>"#,
    );
    let fallback = inline_graphic(
        "3D Model 1",
        "http://schemas.openxmlformats.org/drawingml/2006/picture",
        r#"<pic:pic><pic:nvPicPr><pic:cNvPr id="2" name="3D Model 1"/><pic:cNvPicPr/></pic:nvPicPr><pic:blipFill><a:blip r:embed="rIdPng"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill><pic:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="2743200" cy="1828800"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></pic:spPr></pic:pic>"#,
    );
    let body = format!(
        r#"<w:p><w:r><w:t>A model:</w:t></w:r></w:p><w:p><w:r><mc:AlternateContent><mc:Choice Requires="am3d">{choice}</mc:Choice><mc:Fallback>{fallback}</mc:Fallback></mc:AlternateContent></w:r></w:p><w:p><w:r><w:t>Caption.</w:t></w:r></w:p>"#
    );
    package(
        MAIN,
        &body,
        &[
            ("rIdGlb", "http://schemas.microsoft.com/office/2017/06/relationships/model3d", "media/model3d1.glb"),
            ("rIdPng", "image", "media/image1.png"),
        ],
        &[],
        vec![("word/media/model3d1.glb".into(), b"glTF\x02\x00\x00\x00binary-model".to_vec()), ("word/media/image1.png".into(), PNG.to_vec())],
    )
}

/// A header and footer with fields and future markup, and footnotes.
pub fn headers_notes() -> Vec<u8> {
    let body = concat!(
        r#"<w:p><w:r><w:t>Body with a note</w:t></w:r><w:r><w:rPr><w:rStyle w:val="FootnoteReference"/><w:vertAlign w:val="superscript"/></w:rPr><w:footnoteReference w:id="1"/></w:r><w:r><w:t>.</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:sectPr><w:headerReference w:type="default" r:id="rIdH"/><w:footerReference w:type="default" r:id="rIdF"/><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/></w:sectPr></w:pPr><w:r><w:t>End of section one.</w:t></w:r></w:p>"#,
        r#"<w:p><w:r><w:t>Section two.</w:t></w:r></w:p>"#
    );
    let hdr = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:hdr {NS}><w:p><w:pPr><w:jc w:val="right"/></w:pPr><w:r><w:t xml:space="preserve">Confidential </w:t></w:r><fut:headerMark fut:v="1"/><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText xml:space="preserve"> PAGE </w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>1</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p></w:hdr>"#
    );
    let ftr = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:ftr {NS}><w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:t xml:space="preserve">Page </w:t></w:r><w:fldSimple w:instr=" PAGE "><w:r><w:t>1</w:t></w:r></w:fldSimple><w:r><w:t xml:space="preserve"> of </w:t></w:r><w:fldSimple w:instr=" NUMPAGES "><w:r><w:t>1</w:t></w:r></w:fldSimple></w:p></w:ftr>"#
    );
    let notes = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:footnotes {NS}><w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:separator/></w:r></w:p></w:footnote><w:footnote w:type="continuationSeparator" w:id="0"><w:p><w:r><w:continuationSeparator/></w:r></w:p></w:footnote><w:footnote w:id="1"><w:p><w:r><w:rPr><w:rStyle w:val="FootnoteReference"/><w:vertAlign w:val="superscript"/></w:rPr><w:footnoteRef/></w:r><w:r><w:t xml:space="preserve"> A note with </w:t></w:r><fut:noteMark/><w:r><w:rPr><w14:ligatures w14:val="all"/></w:rPr><w:t>effects</w:t></w:r><w:r><w:t>.</w:t></w:r></w:p></w:footnote></w:footnotes>"#
    );
    package(
        MAIN,
        body,
        &[("rIdH", "header", "header1.xml"), ("rIdF", "footer", "footer1.xml"), ("rIdN", "footnotes", "footnotes.xml")],
        &[
            ("word/header1.xml", "application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml"),
            ("word/footer1.xml", "application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml"),
            ("word/footnotes.xml", "application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml"),
        ],
        vec![
            ("word/header1.xml".into(), hdr.into_bytes()),
            ("word/footer1.xml".into(), ftr.into_bytes()),
            ("word/footnotes.xml".into(), notes.into_bytes()),
        ],
    )
}

/// A macro-enabled document: the VBA project and its data part, never run.
pub fn macros() -> Vec<u8> {
    let body = r#"<w:p><w:r><w:t>This document carries macros.</w:t></w:r></w:p>"#;
    let vba_data = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><wne:vbaSuppData xmlns:wne="http://schemas.microsoft.com/office/word/2006/wordml"><wne:mcds><wne:mcd wne:macroName="PROJECT.MODULE1.HELLO" wne:name="Project.Module1.Hello" wne:bEncrypt="00" wne:cmg="56"/></wne:mcds></wne:vbaSuppData>"#;
    package(
        "application/vnd.ms-word.document.macroEnabled.main+xml",
        body,
        &[("rIdVba", "http://schemas.microsoft.com/office/2006/relationships/vbaProject", "vbaProject.bin")],
        &[("word/vbaData.xml", "application/vnd.ms-word.vbaData+xml"), ("word/vbaProject.bin", "application/vnd.ms-office.vbaProject")],
        vec![
            ("word/vbaProject.bin".into(), b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1opaque macro project".to_vec()),
            (
                "word/_rels/vbaProject.bin.rels".into(),
                rels(&[("rId1", "http://schemas.microsoft.com/office/2006/relationships/wordVbaData", "vbaData.xml")]),
            ),
            ("word/vbaData.xml".into(), s(vba_data)),
        ],
    )
}

/// An imported chunk (`w:altChunk` and its HTML part), unmodelled paragraph properties
/// (`w:divId`, `w:cnfStyle`), custom XML around a table, and printer settings on the section.
pub fn misc() -> Vec<u8> {
    let body = concat!(
        r#"<w:p><w:pPr><w:divId w:val="1234567"/></w:pPr><w:r><w:t>From a web page.</w:t></w:r></w:p>"#,
        r#"<w:altChunk r:id="rIdChunk"/>"#,
        r#"<w:tbl><w:tblPr><w:tblW w:w="0" w:type="auto"/></w:tblPr><w:tblGrid><w:gridCol w:w="4000"/></w:tblGrid><w:tr><w:tc><w:tcPr><w:tcW w:w="4000" w:type="dxa"/></w:tcPr><w:customXml w:element="cell"><w:p><w:pPr><w:cnfStyle w:val="100000000000"/></w:pPr><w:r><w:t>In a table</w:t></w:r></w:p></w:customXml></w:tc></w:tr></w:tbl>"#,
        r#"<w:p><w:r><w:t>Closing.</w:t></w:r></w:p>"#,
        r#"<w:p><w:pPr><w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440" w:header="720" w:footer="720" w:gutter="0"/><w:paperSrc w:first="7" w:other="7"/><w:bidi/><w:rtlGutter/><w:printerSettings r:id="rIdPrn"/></w:sectPr></w:pPr><w:r><w:t>Section with printer settings.</w:t></w:r></w:p>"#
    );
    let chunk = "<html><body><p>Imported <b>HTML</b> chunk.</p></body></html>";
    package(
        MAIN,
        body,
        &[("rIdChunk", "aFChunk", "chunk1.htm"), ("rIdPrn", "printerSettings", "printerSettings/printerSettings1.bin")],
        &[
            ("word/chunk1.htm", "application/xhtml+xml"),
            ("word/printerSettings/printerSettings1.bin", "application/vnd.openxmlformats-officedocument.wordprocessingml.printerSettings"),
        ],
        vec![("word/chunk1.htm".into(), s(chunk)), ("word/printerSettings/printerSettings1.bin".into(), b"DEVMODE-bytes".to_vec())],
    )
}

/// Every corpus document: (file name, package).
pub fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("tracked-moves-comments.docx", tracked()),
        ("smartart.docx", smart_art()),
        ("chart.docx", chart()),
        ("custom-xml.docx", custom_xml()),
        ("w14-future-markup.docx", w14_markup()),
        ("ole-object.docx", ole()),
        ("model-3d.docx", model_3d()),
        ("headers-footers-notes.docx", headers_notes()),
        ("macros.docm", macros()),
        ("altchunk-printer-misc.docx", misc()),
    ]
}
